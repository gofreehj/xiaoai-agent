mod agent;
mod airplay;
mod asr;
mod audio;
mod brain;
// Legacy `record_utterance_server_vad_streaming` wrapper is superseded by the `_outcome` API.
#[allow(dead_code)]
mod capture;
mod config;
mod control;
mod device;
mod dual;
mod frontend;
mod mcp;
mod mcp_legacy_sse;
mod music;
mod realtime;
mod runtime;
mod server;
mod session;
mod shell;
mod speech;
mod tools;
mod tts;
mod vad;
mod weather;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout, MissedTickBehavior};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::agent::AgentRuntime;
use crate::airplay::AirPlayService;
use crate::asr::AsrClient;
use crate::audio::{AudioCoordinator, FinishMode};
use crate::config::{AppConfig, ConfigHandle};
use crate::device::Device;
use crate::frontend::mipns::{reject_orphaned_turn, MipnsFrontend};
use crate::frontend::FrontendEvent;
use crate::music::MusicService;
use crate::runtime::fault::{
    launcher_path, yield_after_consecutive_failures, yield_to_native,
    YIELD_AFTER_CONSECUTIVE_FAILURES,
};
use crate::runtime::reload::{spawn_config_reload, spawn_sighup_listener, ReloadGate};
use crate::runtime::telemetry::{AgentPhase, RuntimeTelemetry};
use crate::session::pipeline::InFlightTurn;
use crate::session::pipeline::{TurnPipeline, TurnPipelineRequest};
use crate::session::{ControlCommand, InterruptCause, SessionOrchestrator};
use crate::speech::{SocketOwnership, SpeechLiveness, SpeechTurn};
use crate::tts::{PlaybackGate, TtsHealth, TtsRouter};

#[derive(Debug, Parser)]
#[command(name = "xiaoai-agent")]
#[command(about = "Standalone XiaoAI on-device agent using the native mipns speech frontend")]
struct Cli {
    #[arg(
        short,
        long,
        default_value = "/data/open-xiaoai/agent.yaml",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum CliCommand {
    /// Inspect agent status, socket ownership, latency metrics, and upstream connectivity.
    Status,
    /// Preflight check: config warnings, socket/PID state, /tmp space, upstream connectivity.
    Doctor {
        /// Record 3 seconds of raw ALSA capture and inspect per-channel bit width / RMS / peak.
        #[arg(long)]
        probe_mic: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.command == Some(CliCommand::Status) {
        return run_status_command(&cli.config).await;
    }
    if let Some(CliCommand::Doctor { probe_mic }) = cli.command {
        return run_doctor_command(&cli.config, probe_mic).await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = ConfigHandle::new(Arc::new(
        AppConfig::load(&cli.config)
            .with_context(|| format!("failed to load config {}", cli.config.display()))?,
    ));
    // 启动期快照：socket / device / asr / airplay / server / mode 按「改完要重启」处理，
    // 其余段通过 ConfigHandle 热加载。
    let initial = config.get().await;
    for warning in crate::config::collect_startup_warnings(&initial) {
        warn!("配置自检: {warning}");
    }

    let device = Device::new(initial.device.clone());
    let music = Arc::new(MusicService::new(config.clone(), device.clone())?);
    let agent = Arc::new(AgentRuntime::new(config.clone(), device.clone(), music.clone()).await?);
    let asr = AsrClient::new(initial.asr.clone())?;
    let airplay = AirPlayService::start(initial.airplay.clone()).await?;
    let telemetry = RuntimeTelemetry::default();
    let playback = Arc::new(PlaybackGate::default());
    let tts = TtsRouter::new(device.clone(), playback.clone());
    let tts_health = Arc::new(TtsHealth::default());
    let audio = AudioCoordinator::new(
        device.clone(),
        initial.device.clone(),
        playback,
        tts,
        music,
        airplay,
        telemetry.clone(),
        tts_health,
    );

    let (reload_tx, mut reload_rx) = mpsc::channel::<()>(1);
    spawn_sighup_listener(reload_tx.clone());
    let reload_gate = Arc::new(ReloadGate::default());
    let (command_tx, mut command_rx) = mpsc::channel::<ControlCommand>(16);
    let hooks = server::ControlHooks {
        command_tx: Some(command_tx),
        audio: Some(audio.clone()),
        music: Some(audio.music.clone()),
        agent: Some(agent.clone()),
        config: Some(config.clone()),
        reload_tx: Some(reload_tx.clone()),
    };
    let mut control_runner =
        ControlRunner::new(config.clone(), agent.clone(), audio.clone(), reload_tx);

    if initial.runtime.mode == "dual" {
        info!("xiaoai-agent 启动模式: 内置外部 KWS 双助手共存模式 (mode=dual)");
        let native_monitor = crate::frontend::native_monitor::NativeWakeMonitor::spawn(
            &initial.dual.native_interlock,
        );
        let ep = initial.dual.kws.endpoint.trim();
        let control_state = Arc::new(server::ControlState {
            token: initial.server.token.clone(),
            mode: "dual".to_string(),
            socket: SocketOwnership::unattributed(PathBuf::from(if ep.is_empty() {
                "external-kws".to_string()
            } else {
                format!("udp://{ep}")
            })),
            liveness: SpeechLiveness::default(),
            native_monitor: Some(native_monitor.clone()),
            started_at: std::time::Instant::now(),
            config_path: Some(cli.config.clone()),
            log_path: None,
            telemetry: telemetry.clone(),
        });
        let _control_task = start_control_plane(&initial.server, control_state, hooks).await;
        let mut control_native_rx = native_monitor.subscribe();
        let control_native_monitor = native_monitor.clone();
        // dual 模式下控制面任务同样监听原生小爱唤醒，命中时立刻 abort /speak 与 /chat。
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    maybe_cmd = command_rx.recv() => {
                        let Some(cmd) = maybe_cmd else { break; };
                        if matches!(cmd, ControlCommand::Speak(_) | ControlCommand::Chat(_))
                            && control_native_monitor.is_blocked()
                        {
                            warn!("原生小爱活跃或冷却保护中，跳过控制面语音请求");
                            continue;
                        }
                        control_runner.handle(cmd).await;
                    }
                    _ = crate::frontend::native_monitor::wait_native_wake(&mut control_native_rx) => {
                        if control_runner.abort_running() {
                            info!("检测到原生小爱唤醒，已打断控制面播报/对话任务");
                            control_runner.audio.yield_to_native_dialog(true).await;
                        }
                    }
                }
            }
        });
        return dual::run_dual_daemon(
            config,
            cli.config.clone(),
            asr,
            agent,
            audio,
            reload_rx,
            reload_gate,
            native_monitor,
        )
        .await;
    }

    let mut speech = MipnsFrontend::bind(&initial.runtime.speech_socket).await?;
    let speech_handle = speech.speech_handle();
    let ownership = speech.ownership();

    let control_state = Arc::new(server::ControlState {
        token: initial.server.token.clone(),
        mode: initial.runtime.mode.clone(),
        socket: ownership.clone(),
        liveness: speech.liveness(),
        native_monitor: None,
        started_at: std::time::Instant::now(),
        config_path: Some(cli.config.clone()),
        log_path: None,
        telemetry: telemetry.clone(),
    });
    let _control_task = start_control_plane(&initial.server, control_state, hooks).await;

    let failures = Arc::new(AtomicU32::new(0));
    let base_orchestrator = SessionOrchestrator::new(
        config.clone(),
        asr.clone(),
        agent.clone(),
        audio.clone(),
        failures.clone(),
    );

    let launcher = launcher_path();
    if launcher.is_none() {
        warn!("找不到 start-agent.sh：故障时无法主动让位，只能退出等 watchdog 重启");
    }

    info!(socket = %initial.runtime.speech_socket, "xiaoai-agent ready");
    device
        .blink_ready(initial.device.led_listening, Duration::from_millis(250))
        .await;

    let mut active_session: Option<ActiveSession> = None;
    let mut session_check = interval(Duration::from_millis(250));
    session_check.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            event = speech.recv_event() => {
                let Some(event) = event else {
                    let outcome = yield_to_native(
                        &device,
                        launcher.as_deref(),
                        &ownership,
                        "speech.usock service stopped",
                    )
                    .await;
                    std::process::exit(if outcome.succeeded() { 0 } else { 1 });
                };
                match event {
                    FrontendEvent::Registered { vendor, codec } => {
                        info!(vendor, codec = codec.as_deref().unwrap_or("pcm16"), "speech frontend registered");
                    }
                    FrontendEvent::Fatal(message) => {
                        error!(reason = %message, "speech frontend reported a fatal error");
                        if let Some(active) = active_session.take() {
                            let interrupted = active.turn_id.load(Ordering::Relaxed);
                            if !active.task.is_finished() {
                                active.task.abort();
                            }
                            let _ = active.task.await;
                            let _ = timeout(
                                Duration::from_secs(2),
                                base_orchestrator.interrupt_mipns_turn(
                                    &speech_handle,
                                    interrupted,
                                    InterruptCause::Failure("speech service fatal"),
                                ),
                            )
                            .await;
                        }
                        let outcome = yield_to_native(
                            &device,
                            launcher.as_deref(),
                            &ownership,
                            "speech.usock fatal",
                        )
                        .await;
                        std::process::exit(if outcome.succeeded() { 0 } else { 1 });
                    }
                    FrontendEvent::Wake(frontend_turn) => {
                        let Some(turn) = frontend_turn.into_speech_turn() else {
                            continue;
                        };
                        info!(
                            turn_id = turn.id(),
                            interaction_mode = turn.interaction_mode(),
                            "WAKE native mipns"
                        );
                        // 1. 立即 abort 旧会话任务并同步掐掉正在发声的播放器/暂停音乐。
                        let prev_active = active_session.take();
                        if let Some(ref active) = prev_active {
                            if !active.task.is_finished() {
                                active.task.abort();
                            }
                        }
                        audio.interrupt_for_wake().await;

                        // 2. 声音已停，后台并发预热 ASR 与 LLM 连接池，并收尾上一轮协议与打断上下文。
                        asr.prewarm();
                        agent.prewarm().await;
                        if let Some(active) = prev_active {
                            let interrupted = active.turn_id.load(Ordering::Relaxed);
                            let _ = active.task.await;
                            if let Some((user_q, partial_ans)) = active.in_flight.take() {
                                agent.record_partial_turn(&user_q, &partial_ans).await;
                            }
                            base_orchestrator
                                .interrupt_mipns_turn(
                                    &speech_handle,
                                    interrupted,
                                    InterruptCause::Wake,
                                )
                                .await;
                        } else {
                            agent.reset_session("wake keyword").await;
                        }
                        audio.set_phase(AgentPhase::Idle).await;
                        audio.set_phase(AgentPhase::Listening).await;

                        let (followup_tx, followup_rx) = mpsc::channel(2);
                        let handle = speech_handle.clone();
                        let turn_id = Arc::new(AtomicU64::new(turn.id()));
                        let task_turn_id = turn_id.clone();
                        let mut session_orch = base_orchestrator.clone();
                        session_orch.in_flight = InFlightTurn::default();
                        let session_in_flight = session_orch.in_flight.clone();
                        let task = tokio::spawn(async move {
                            if let Err(err) = session_orch
                                .run_mipns_turn(handle, turn, followup_rx, task_turn_id)
                                .await
                            {
                                error!("turn failed: {err:?}");
                            }
                        });
                        active_session = Some(ActiveSession {
                            task,
                            followup_tx,
                            turn_id,
                            in_flight: session_in_flight,
                        });
                    }
                    FrontendEvent::FollowUp(frontend_turn) => {
                        let Some(turn) = frontend_turn.into_speech_turn() else {
                            continue;
                        };
                        info!(
                            turn_id = turn.id(),
                            activation = ?turn.activation(),
                            interaction_mode = turn.interaction_mode(),
                            "FOLLOW_UP native mipns"
                        );
                        let receiver_ready = active_session
                            .as_ref()
                            .is_some_and(|active| !active.task.is_finished());
                        if receiver_ready {
                            let send_result = active_session
                                .as_ref()
                                .expect("active session was checked")
                                .followup_tx
                                .send(turn)
                                .await;
                            if let Err(err) = send_result {
                                reject_orphaned_turn(err.0, &speech_handle).await;
                            }
                        } else {
                            reject_orphaned_turn(turn, &speech_handle).await;
                        }
                    }
                    FrontendEvent::ExitCmd => {}
                }
            }
            Some(()) = reload_rx.recv() => {
                spawn_config_reload(&config, cli.config.clone(), &agent, &reload_gate);
            }
            Some(cmd) = command_rx.recv() => {
                let stopping = matches!(cmd, ControlCommand::Stop);
                if stopping {
                    // /stop 同时打断进行中的语音会话（先 abort，再收尾协议）。
                    if let Some(active) = active_session.take() {
                        let interrupted = active.turn_id.load(Ordering::Relaxed);
                        if !active.task.is_finished() {
                            active.task.abort();
                        }
                        let _ = active.task.await;
                        if let Some((user_q, partial_ans)) = active.in_flight.take() {
                            agent.record_partial_turn(&user_q, &partial_ans).await;
                        }
                        let _ = timeout(
                            Duration::from_secs(2),
                            base_orchestrator.interrupt_mipns_turn(
                                &speech_handle,
                                interrupted,
                                InterruptCause::FrontendStopped,
                            ),
                        )
                        .await;
                    }
                }
                control_runner.handle(cmd).await;
            }
            _ = session_check.tick() => {
                let session_finished = active_session
                    .as_ref()
                    .is_some_and(|active| active.task.is_finished());
                if session_finished {
                    if let Some(active) = active_session.take() {
                        if let Err(err) = active.task.await {
                            warn!("turn task ended unexpectedly: {err:?}");
                        }
                    }
                    telemetry.set_phase(AgentPhase::Idle);
                    let streak = failures.load(Ordering::Relaxed);
                    if streak >= YIELD_AFTER_CONSECUTIVE_FAILURES {
                        error!(
                            consecutive_failures = streak,
                            "连续多轮会话失败，故障让位给原厂小爱"
                        );
                        if let Some(code) = yield_after_consecutive_failures(
                            &device,
                            launcher.as_deref(),
                            &ownership,
                            &failures,
                        )
                        .await
                        {
                            std::process::exit(code);
                        }
                    }
                    device
                        .blink_ready(initial.device.led_listening, Duration::from_millis(250))
                        .await;
                }
            }
        }
    }
}

struct ActiveSession {
    task: JoinHandle<()>,
    followup_tx: mpsc::Sender<SpeechTurn>,
    turn_id: Arc<AtomicU64>,
    in_flight: InFlightTurn,
}

/// 控制面命令执行器：`/speak`、`/chat` 在后台任务里跑，新命令或 `/stop` 会先打断上一条。
struct ControlRunner {
    config: ConfigHandle,
    agent: Arc<AgentRuntime>,
    audio: AudioCoordinator,
    in_flight: InFlightTurn,
    task: Option<JoinHandle<()>>,
    reload_tx: mpsc::Sender<()>,
    next_turn_id: Arc<AtomicU64>,
}

/// 控制面发起的轮次 id 从高位起算，避免和 mipns 的 turn id 撞车。
const CONTROL_TURN_ID_BASE: u64 = 1 << 40;

impl ControlRunner {
    fn new(
        config: ConfigHandle,
        agent: Arc<AgentRuntime>,
        audio: AudioCoordinator,
        reload_tx: mpsc::Sender<()>,
    ) -> Self {
        Self {
            config,
            agent,
            audio,
            in_flight: InFlightTurn::default(),
            task: None,
            reload_tx,
            next_turn_id: Arc::new(AtomicU64::new(CONTROL_TURN_ID_BASE)),
        }
    }

    fn abort_running(&mut self) -> bool {
        if let Some(task) = self.task.take() {
            if !task.is_finished() {
                task.abort();
                return true;
            }
        }
        false
    }

    async fn handle(&mut self, cmd: ControlCommand) {
        match cmd {
            ControlCommand::Stop => {
                self.abort_running();
                self.audio.stop_all_output().await;
            }
            ControlCommand::Speak(text) => {
                self.abort_running();
                let audio = self.audio.clone();
                let config = self.config.clone();
                self.task = Some(tokio::spawn(async move {
                    let cfg = config.get().await;
                    if let Err(err) = audio
                        .speak_once(&cfg.tts, &text, FinishMode::WaitForEnd)
                        .await
                    {
                        warn!("控制面 /speak 播报失败: {err:#}");
                    }
                    audio.set_phase(AgentPhase::Idle).await;
                }));
            }
            ControlCommand::Chat(text) => {
                self.abort_running();
                let audio = self.audio.clone();
                let agent = self.agent.clone();
                let config = self.config.clone();
                let in_flight = self.in_flight.clone();
                let turn_id = self.next_turn_id.fetch_add(1, Ordering::Relaxed);
                self.task = Some(tokio::spawn(async move {
                    let cfg = config.get().await;
                    audio.set_phase(AgentPhase::Thinking).await;
                    let pipeline = TurnPipeline::new(&agent, &audio, &in_flight);
                    let res = pipeline
                        .execute(TurnPipelineRequest {
                            config: &cfg,
                            turn_id,
                            source: "web_chat",
                            query: &text,
                            asr_started_at: std::time::Instant::now(),
                            asr_ms: 0,
                            vad_silence_ms: None,
                            speech_voiced_ms: None,
                            pcm_bytes: None,
                            finish_mode: FinishMode::WaitForEnd,
                        })
                        .await;
                    if let Err(err) = res {
                        warn!("控制面 /chat 轮次失败: {err:#}");
                    }
                    let _ = in_flight.take();
                    audio.set_phase(AgentPhase::Idle).await;
                }));
            }
            ControlCommand::Reload => {
                // 通道容量 1：已有待处理的重载时合并，不阻塞命令循环。
                let _ = self.reload_tx.try_send(());
            }
            ControlCommand::ClearHistory => {
                self.agent.reset_session("clear history").await;
            }
            ControlCommand::Volume(action) => {
                if let Err(err) = self.audio.execute_volume(action).await {
                    warn!("控制面 /volume 执行失败: {err:#}");
                }
            }
        }
    }
}

/// 起控制面。**失败不返回 Err**：配置错误只该废掉控制面，不该废掉音箱。
async fn start_control_plane(
    config: &crate::config::ServerConfig,
    state: Arc<server::ControlState>,
    hooks: server::ControlHooks,
) -> Option<tokio::task::JoinHandle<()>> {
    match server::spawn_with_hooks(config, state, hooks).await {
        Ok(task) => task,
        Err(err) => {
            error!("控制面启动失败（语音功能不受影响）: {err:#}");
            warn!("控制面已禁用：本机没有 /health 可用（改 agent.yaml 的 server 段并重启）");
            None
        }
    }
}

async fn run_status_command(config_path: &Path) -> anyhow::Result<()> {
    let config = AppConfig::load(config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    println!("config: {}", config_path.display());
    println!("mode: {}", config.runtime.mode);
    println!(
        "asr: {:?} | llm: {:?} ({}) | tts: {:?}",
        config.asr.provider, config.llm.protocol, config.llm.model, config.tts.provider
    );

    let pid_file = Path::new("/tmp/xiaoai-agent/xiaoai-agent.pid");
    let pid_text = std::fs::read_to_string(pid_file)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    println!("pid_file: {}", pid_text.as_deref().unwrap_or("none"));

    if config.runtime.mode == "dual" {
        let ep = config.dual.kws.endpoint.trim();
        println!(
            "external_kws: {}",
            if ep.is_empty() { "<unconfigured>" } else { ep }
        );
    } else {
        let sock_exists = Path::new(&config.runtime.speech_socket).exists();
        println!(
            "speech_socket: {} (exists={})",
            config.runtime.speech_socket, sock_exists
        );
    }

    if config.server.enabled {
        let connect_host = match config.server.host.as_str() {
            "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
            other => other,
        };
        let url = format!("http://{connect_host}:{}/status", config.server.port);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?;
        let mut req = client.get(&url);
        if !config.server.token.trim().is_empty() {
            req = req.bearer_auth(config.server.token.trim());
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await?;
                println!(
                    "control_plane: ok (phase={}, uptime_s={}, socket_owned={})",
                    body["phase"].as_str().unwrap_or("unknown"),
                    body["uptime_s"].as_u64().unwrap_or(0),
                    body["socket"]["owned"].as_bool().unwrap_or(false)
                );
                if let Some(lt) = body.get("last_turn").and_then(|v| v.as_object()) {
                    println!(
                        "last_turn: turn_id={} asr_ms={} llm_ttft_ms={} llm_total_ms={} tts_ms={} e2e_first_audio_ms={}",
                        lt.get("turn_id").and_then(|v| v.as_u64()).unwrap_or(0),
                        lt.get("asr_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                        lt.get("llm_ttft_ms").and_then(|v| v.as_u64()).map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                        lt.get("llm_total_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                        lt.get("tts_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                        lt.get("e2e_first_audio_ms").and_then(|v| v.as_u64()).map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                    );
                }
            }
            Ok(resp) => {
                println!("control_plane: HTTP {}", resp.status());
            }
            Err(err) => {
                println!("control_plane: unreachable ({err})");
            }
        }
    } else {
        println!("control_plane: disabled (server.enabled=false)");
    }

    let probe_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .build()?;
    let llm_ok = probe_client
        .head(config.llm.base_url.trim_end_matches('/'))
        .send()
        .await
        .is_ok();
    let asr_url = match config.asr.provider {
        crate::config::AsrProvider::OpenAi => config.asr.open_ai.base_url.trim_end_matches('/'),
        crate::config::AsrProvider::OpenAiRealtime => {
            config.asr.openai_realtime.base_url.trim_end_matches('/')
        }
    };
    let asr_ok = probe_client.head(asr_url).send().await.is_ok();
    println!(
        "upstream: asr={} llm={}",
        if asr_ok { "reachable" } else { "unreachable" },
        if llm_ok { "reachable" } else { "unreachable" }
    );
    Ok(())
}

async fn run_doctor_command(config_path: &Path, probe_mic: bool) -> anyhow::Result<()> {
    let config = AppConfig::load(config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let report = crate::runtime::doctor::collect_doctor_report(&config, Some(config_path), None).await;

    println!("config: {} (parsed ok)", config_path.display());
    println!("mode: {}", report.mode);
    println!("dialogue_engine: {}", report.dialogue_engine);
    println!("realtime_provider: {}", report.realtime_provider);

    if report.warnings.is_empty() {
        println!("warnings: none");
    } else {
        println!("warnings: {}", report.warnings.len());
        for warning in &report.warnings {
            println!("  WARN {warning}");
        }
    }

    println!("pid_file: {}", report.pid_file.as_deref().unwrap_or("none"));
    if config.runtime.mode == "dual" {
        let ep = config.dual.kws.endpoint.trim();
        println!(
            "external_kws: {}",
            if ep.is_empty() { "<unconfigured>" } else { ep }
        );
        println!(
            "dual_capture: device={} rate={}Hz ch={} format={:?} mic_ch={} mic_channels={:?} ref_ch={:?} shift_bits={} kws_agc={}",
            config.dual.capture.device,
            config.dual.capture.sample_rate,
            config.dual.capture.channels,
            config.dual.capture.format,
            config.dual.capture.mic_channel,
            config.dual.capture.mic_channels,
            config.dual.capture.ref_channel,
            config.dual.capture.shift_bits,
            config.dual.capture.kws_agc_enabled,
        );
        let ni = &config.dual.native_interlock;
        let syslog_desc = ni.syslog_path.as_ref().map_or_else(
            || "none".to_string(),
            |p| format!("{} (exists={})", p.display(), p.exists()),
        );
        let inst_desc = ni.instruction_log_path.as_ref().map_or_else(
            || "none".to_string(),
            |p| format!("{} (exists={})", p.display(), p.exists()),
        );
        let event_cmd = if ni.event_command.trim().is_empty() {
            "<none>"
        } else {
            ni.event_command.trim()
        };
        println!(
            "native_interlock: enabled={} event_command={} syslog={} instruction_log={} max_active={}s cooldown={}s",
            ni.enabled, event_cmd, syslog_desc, inst_desc, ni.max_active_s, ni.cooldown_s
        );
    } else {
        let socket = &config.runtime.speech_socket;
        println!("socket: {socket} (exists={})", Path::new(socket).exists());
    }

    match report.tmp_free_kib {
        Some(kib) => {
            let note = if kib < 4096 { " LOW" } else { "" };
            println!("tmp_free: {} KiB{note}", kib);
        }
        None => println!("tmp_free: unknown"),
    }
    match report.data_free_kib {
        Some(kib) => {
            let note = if kib < 2048 { " LOW (<2 MiB)" } else { "" };
            println!("data_free ({}): {} KiB{note}", report.data_path, kib);
        }
        None => println!("data_free ({}): unknown", report.data_path),
    }

    println!("upstream:");
    for u in &report.upstreams {
        if u.label == "external_kws" || u.label == "realtime_ws" {
            if u.reachable {
                println!("  {}: reachable ({} ms) {}", u.label, u.rtt_ms, u.url);
            } else {
                println!(
                    "  {}: UNREACHABLE ({}) {}",
                    u.label,
                    u.error.as_deref().unwrap_or("unknown"),
                    u.url
                );
            }
        } else if u.reachable {
            println!(
                "  {}: reachable ({} ms, HTTP {}) {}",
                u.label,
                u.rtt_ms,
                u.status_code.unwrap_or(0),
                u.url
            );
        } else {
            println!(
                "  {}: UNREACHABLE ({}) {}",
                u.label,
                u.error.as_deref().unwrap_or("unknown"),
                u.url
            );
        }
    }

    if probe_mic {
        run_probe_mic(&config).await?;
    }
    Ok(())
}

async fn run_probe_mic(config: &AppConfig) -> anyhow::Result<()> {
    let cap = &config.dual.capture;
    if cap.format != crate::config::DualCaptureFormat::S32Le {
        anyhow::bail!(
            "--probe-mic 仅支持 S32_LE 多通道诊断，当前 dual.capture.format={:?}",
            cap.format
        );
    }
    println!(
        "probe_mic: recording 3s from device={} rate={}Hz channels={} format=S32_LE ...",
        cap.device, cap.sample_rate, cap.channels
    );
    let output = tokio::time::timeout(
        Duration::from_secs(7),
        tokio::process::Command::new("arecord")
            .args([
                "-q",
                "-D",
                &cap.device,
                "-t",
                "raw",
                "-f",
                "S32_LE",
                "-r",
                &cap.sample_rate.to_string(),
                "-c",
                &cap.channels.to_string(),
                "-d",
                "3",
            ])
            .output(),
    )
    .await
    .context("arecord --probe-mic 超时")?
    .context("执行 arecord 失败")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("arecord 返回非零状态 ({}): {}", output.status, stderr.trim());
    }

    let stats = crate::frontend::external_kws::probe_capture_channels(
        &output.stdout,
        cap.channels as usize,
    )?;
    println!("probe_mic_channels ({} bytes captured):", output.stdout.len());
    for ch in stats {
        println!(
            "  ch{}: active={} samples={} peak_abs={} rms={:.1} outside_s24={} low_byte_zero={} full_scale_32={} suggested_shift={}",
            ch.channel,
            ch.active,
            ch.samples,
            ch.peak_abs,
            ch.rms,
            ch.outside_signed_24,
            ch.low_byte_zero,
            ch.full_scale_32,
            ch.suggested_shift_bits
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::Mutex;

    use crate::config::{AppConfig, DeviceConfig};
    use crate::frontend::mipns::{drain_stale_followup_turns, request_followup_capture};
    use crate::runtime::fault::{
        YieldOutcome, LAUNCHER_EXIT_HALF_DISMANTLED, LAUNCHER_EXIT_UNTOUCHED,
    };
    use crate::runtime::reload::{apply_config_reload, ReloadGuard};
    use crate::session::{
        handle_local_intent, is_capture_timeout, LocalIntentContext, LocalIntentOutcome,
        EXIT_FAREWELL_PROMPT, WAKE_ACK_PROMPT,
    };
    use crate::shell::RecordingShell;
    use crate::speech::{SpeechInput, TestCommand};

    fn base_test_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.agent.tools_enabled = false;
        config.airplay.enabled = false;
        config.runtime.session_idle_timeout_s = 1.0;
        config
    }

    async fn test_orchestrator(shell: Arc<RecordingShell>) -> SessionOrchestrator {
        test_orchestrator_with(base_test_config(), shell).await
    }

    async fn test_orchestrator_with(
        config: AppConfig,
        shell: Arc<RecordingShell>,
    ) -> SessionOrchestrator {
        let config = ConfigHandle::new(Arc::new(config));
        let initial = config.get().await;
        let device = Device::with_runner(initial.device.clone(), shell);
        let asr = AsrClient::new(initial.asr.clone()).expect("默认 ASR 配置应当可用");
        let music = Arc::new(
            MusicService::new(config.clone(), device.clone()).expect("音乐服务构造不应失败"),
        );
        let airplay = AirPlayService::start(initial.airplay.clone())
            .await
            .expect("airplay.enabled=false 时不应失败");
        let agent = Arc::new(
            AgentRuntime::new(config.clone(), device.clone(), music.clone())
                .await
                .expect("离线构造 agent 不应失败"),
        );
        let playback = Arc::new(PlaybackGate::default());
        let tts = TtsRouter::new(device.clone(), playback.clone());
        let audio = AudioCoordinator::new(
            device,
            initial.device.clone(),
            playback,
            tts,
            music,
            airplay,
            RuntimeTelemetry::default(),
            Arc::new(TtsHealth::default()),
        );
        SessionOrchestrator::new(config, asr, agent, audio, Arc::new(AtomicU32::new(0)))
    }

    async fn commands(recorder: &Arc<Mutex<Vec<TestCommand>>>) -> Vec<TestCommand> {
        recorder.lock().await.clone()
    }

    fn followup_channel() -> mpsc::Receiver<SpeechTurn> {
        let (_tx, rx) = mpsc::channel(2);
        rx
    }

    #[tokio::test]
    async fn wake_interrupt_finishes_the_old_dialog_and_resets_the_session() {
        let orch = test_orchestrator(Arc::new(RecordingShell::always_ok())).await;
        let (turn, _input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(7);

        orch.interrupt_mipns_turn(&handle, turn.id(), InterruptCause::Wake)
            .await;

        assert_eq!(
            commands(&recorder).await,
            vec![TestCommand::Finish {
                continue_dialog: false
            }],
            "唤醒打断必须 finish 旧对话"
        );
        assert_eq!(orch.agent.reset_reason_for_test().await, "wake keyword");
    }

    #[tokio::test]
    async fn idle_timeout_interrupt_never_reissues_stop_capture() {
        let orch = test_orchestrator(Arc::new(RecordingShell::always_ok())).await;
        let (turn, _input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(8);

        orch.interrupt_mipns_turn(&handle, turn.id(), InterruptCause::IdleTimeout)
            .await;

        assert_eq!(
            commands(&recorder).await,
            vec![TestCommand::Finish {
                continue_dialog: false
            }],
            "中断收尾只发 finish_dialog，不碰 stop_capture"
        );
        assert_eq!(
            orch.agent.reset_reason_for_test().await,
            "session idle timeout"
        );
    }

    #[tokio::test]
    async fn cancelled_stream_fails_the_turn_but_still_cleans_up() {
        let shell = Arc::new(RecordingShell::always_ok());
        let orch = test_orchestrator(shell.clone()).await;
        let (turn, input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(11);
        input_tx.send(SpeechInput::Cancelled).await.unwrap();
        let turn_id = Arc::new(AtomicU64::new(turn.id()));

        let result = orch
            .run_mipns_turn(handle, turn, followup_channel(), turn_id)
            .await;

        assert!(result.is_err(), "mipns 取消流应当让这一轮失败");
        assert_eq!(
            commands(&recorder).await,
            vec![TestCommand::Finish {
                continue_dialog: false
            }],
            "失败路径只 finish 一次：run_session 里的采集错误不再各自补一条"
        );
        assert_eq!(
            shell.count_matching("/bin/shut_led"),
            4,
            "四个 LED 必须全灭，否则会留一盏常亮灯"
        );
        assert_eq!(orch.agent.reset_reason_for_test().await, "turn failed");
        assert_eq!(
            orch.failures.load(Ordering::Relaxed),
            1,
            "失败轮数要累计：连续 3 轮才让位"
        );
    }

    #[tokio::test]
    async fn idle_timeout_ends_the_session_without_a_failure() {
        let shell = Arc::new(RecordingShell::always_ok());
        let orch = test_orchestrator(shell).await;
        let (turn, _input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(12);
        let turn_id = Arc::new(AtomicU64::new(turn.id()));

        let result = orch
            .run_mipns_turn(handle, turn, followup_channel(), turn_id)
            .await;

        assert!(result.is_ok(), "空闲超时不是失败: {result:?}");
        assert_eq!(
            commands(&recorder).await,
            vec![
                TestCommand::StopCapture,
                TestCommand::Finish {
                    continue_dialog: false
                }
            ],
            "先 stop_capture（capture.rs）再 finish_dialog（收尾）"
        );
        assert_eq!(
            orch.agent.reset_reason_for_test().await,
            "session idle timeout"
        );
        assert_eq!(
            orch.failures.load(Ordering::Relaxed),
            0,
            "正常结束要清零，否则偶发失败会累积成让位"
        );
    }

    #[tokio::test]
    async fn followup_request_command_goes_through_the_seam_and_succeeds() {
        let shell = Arc::new(RecordingShell::answering(r#"{"code":0}"#));
        let device = Device::with_runner(DeviceConfig::default(), shell.clone());

        request_followup_capture(&device).await.unwrap();

        let script = shell.last_script().expect("必须真的调 pnshelper");
        assert!(script.contains("pnshelper event_notify"), "{script}");
        assert!(script.contains(r#"{"src":1,"event":0}"#), "{script}");
    }

    #[tokio::test]
    async fn followup_request_surfaces_command_failures() {
        let shell = Arc::new(RecordingShell::exiting_with(2, "ubus: not found"));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let err = request_followup_capture(&device)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exit=2"), "{err}");
        assert!(err.contains("ubus: not found"), "{err}");

        let shell = Arc::new(RecordingShell::cannot_run(
            "shell command timed out after 10s: ubus",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let err = request_followup_capture(&device)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed to request follow-up capture"), "{err}");
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn followup_request_rejects_bad_pnshelper_answers() {
        let device = Device::with_runner(
            DeviceConfig::default(),
            Arc::new(RecordingShell::answering("not json")),
        );
        let err = request_followup_capture(&device)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid follow-up response"), "{err}");

        let device = Device::with_runner(
            DeviceConfig::default(),
            Arc::new(RecordingShell::answering(r#"{"code":7}"#)),
        );
        let err = request_followup_capture(&device)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("rejected follow-up capture"), "{err}");
    }

    #[test]
    fn interrupt_causes_mark_normal_endings_as_expected() {
        assert!(InterruptCause::Wake.expected());
        assert!(InterruptCause::IdleTimeout.expected());
        assert!(InterruptCause::FrontendStopped.expected());
        assert!(!InterruptCause::Failure("turn failed").expected());

        assert_eq!(
            InterruptCause::FrontendStopped.reason(),
            "speech frontend stopped"
        );
        assert_eq!(InterruptCause::IdleTimeout.reason(), "session idle timeout");
        assert_eq!(
            InterruptCause::Failure("turn failed").reason(),
            "turn failed"
        );
    }

    #[test]
    fn is_capture_timeout_matches_only_the_idle_timeout_error() {
        assert!(is_capture_timeout(&anyhow::anyhow!(
            "timed out waiting for user speech"
        )));
        assert!(!is_capture_timeout(&anyhow::anyhow!(
            "timed out waiting for Realtime ASR transcript"
        )));
        assert!(!is_capture_timeout(&anyhow::anyhow!(
            "mipns cancelled the speech stream"
        )));
    }

    #[tokio::test]
    async fn orphaned_turn_stops_capture_before_finishing_the_dialog() {
        let (turn, _input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(21);

        reject_orphaned_turn(turn, &handle).await;

        assert_eq!(
            commands(&recorder).await,
            vec![
                TestCommand::StopCapture,
                TestCommand::Finish {
                    continue_dialog: false
                }
            ],
            "顺序不能反：先停采集再挂断，否则 mipns 的 stream 一直挂着"
        );
    }

    #[tokio::test]
    async fn yield_to_native_asks_the_launcher_to_stop() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = Device::with_runner(DeviceConfig::default(), shell.clone());
        let launcher = Path::new("/data/open-xiaoai/start-agent.sh");
        let owned = still_owned_socket("yield-ok");

        assert!(yield_to_native(&device, Some(launcher), &owned, "fatal")
            .await
            .succeeded());

        let scripts = shell.detached_scripts();
        assert_eq!(scripts.len(), 1, "让位就一条命令");
        assert!(
            scripts[0].ends_with("start-agent.sh stop"),
            "必须是 launcher 的 stop（restore_native_frontend 在它里面）: {}",
            scripts[0]
        );
        assert!(
            shell.scripts().is_empty(),
            "让位必须走 detached 通道：普通通道的超时会杀掉正在 restore 的 sh"
        );
    }

    #[tokio::test]
    async fn yield_to_native_reports_failure_instead_of_pretending() {
        let shell = Arc::new(RecordingShell::exiting_with(
            1,
            "another launcher operation is in progress",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let launcher = Path::new("/data/open-xiaoai/start-agent.sh");
        let owned = still_owned_socket("yield-fail");

        assert!(!yield_to_native(&device, Some(launcher), &owned, "fatal")
            .await
            .succeeded());
    }

    #[tokio::test]
    async fn yield_to_native_reports_an_unexecutable_command() {
        let shell = Arc::new(RecordingShell::cannot_run(
            "shell command timed out after 180s: start-agent.sh stop",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let launcher = Path::new("/data/open-xiaoai/start-agent.sh");
        let owned = still_owned_socket("yield-timeout");

        assert!(!yield_to_native(&device, Some(launcher), &owned, "fatal")
            .await
            .succeeded());
    }

    #[tokio::test]
    async fn yield_treats_a_taken_over_socket_as_success_even_when_the_command_failed() {
        let shell = Arc::new(RecordingShell::exiting_with(
            1,
            "another launcher operation is in progress",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let launcher = Path::new("/data/open-xiaoai/start-agent.sh");

        let owned = taken_over_socket("yield-taken-over");
        let outcome = yield_to_native(&device, Some(launcher), &owned, "fatal").await;

        assert!(!outcome.issued, "命令本身确实失败了");
        assert!(outcome.taken_over, "但 socket 已经被接管走");
        assert!(outcome.succeeded(), "这种情况必须算让位成功");
        let _ = std::fs::remove_file(owned.path());
    }

    #[tokio::test]
    async fn yield_succeeds_when_the_command_succeeded_even_when_still_owned() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let launcher = Path::new("/data/open-xiaoai/start-agent.sh");
        let owned = still_owned_socket("yield-still-mine");

        let outcome = yield_to_native(&device, Some(launcher), &owned, "fatal").await;

        assert!(outcome.issued);
        assert!(!outcome.taken_over);
        assert!(outcome.succeeded(), "命令成功就算让位达成");
        let _ = std::fs::remove_file(owned.path());
    }

    #[tokio::test]
    async fn consecutive_failures_exit_only_when_the_yield_succeeded() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
        let owned = still_owned_socket("streak-exit");

        let code = yield_after_consecutive_failures(
            &device,
            Some(Path::new("/data/open-xiaoai/start-agent.sh")),
            &owned,
            &failures,
        )
        .await;

        assert_eq!(code, Some(0), "让位成功才退出");
        let _ = std::fs::remove_file(owned.path());
    }

    #[tokio::test]
    async fn consecutive_failures_exit_when_the_launcher_stopped_halfway() {
        for exit in [4_i32, 5, 1] {
            let shell = Arc::new(RecordingShell::exiting_with(exit, "restore failed"));
            let device = Device::with_runner(DeviceConfig::default(), shell);
            let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
            let owned = still_owned_socket(&format!("streak-half-{exit}"));

            let code = yield_after_consecutive_failures(
                &device,
                Some(Path::new("/data/open-xiaoai/start-agent.sh")),
                &owned,
                &failures,
            )
            .await;

            assert_eq!(code, Some(1), "退出码 {exit} 说明前端已被动过，必须退出");
            let _ = std::fs::remove_file(owned.path());
        }
    }

    #[tokio::test]
    async fn exit_code_5_beats_a_replaced_inode() {
        let shell = Arc::new(RecordingShell::exiting_with(
            LAUNCHER_EXIT_HALF_DISMANTLED,
            "failed to restart native mico",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
        let taken = taken_over_socket("half-dismantled");

        let code = yield_after_consecutive_failures(
            &device,
            Some(Path::new("/x/start-agent.sh")),
            &taken,
            &failures,
        )
        .await;

        assert_eq!(
            code,
            Some(1),
            "inode 换了但原生服务没起来，不能算成功（退出码 0 = 哑且无任何信号）"
        );
        let _ = std::fs::remove_file(taken.path());
    }

    #[tokio::test]
    async fn a_misconfigured_control_plane_does_not_take_the_agent_down() {
        use crate::config::ServerConfig;

        let config = ServerConfig {
            enabled: true,
            host: "0.0.0.0".to_string(),
            port: 0,
            token: String::new(),
        };
        let state = Arc::new(crate::server::ControlState {
            token: String::new(),
            mode: "exclusive".to_string(),
            socket: SocketOwnership::unattributed(std::env::temp_dir().join("xiaoai-not-there")),
            liveness: SpeechLiveness::default(),
            native_monitor: None,
            started_at: std::time::Instant::now(),
            config_path: None,
            log_path: None,
            telemetry: RuntimeTelemetry::default(),
        });

        assert!(
            start_control_plane(&config, state, server::ControlHooks::default())
                .await
                .is_none(),
            "控制面起不来应当返回 None 而不是让调用方退出"
        );
    }

    #[test]
    fn exit_code_5_is_never_success_even_with_a_replaced_inode() {
        let half = YieldOutcome {
            issued: false,
            taken_over: true,
            timed_out: false,
            exit_code: Some(LAUNCHER_EXIT_HALF_DISMANTLED),
        };
        assert!(!half.succeeded(), "inode 换了但原生服务没起来，不能算成功");

        let restored = YieldOutcome {
            exit_code: Some(0),
            ..half
        };
        assert!(restored.succeeded());
    }

    #[tokio::test]
    async fn consecutive_failures_exit_when_the_yield_timed_out() {
        let shell = Arc::new(RecordingShell::cannot_run(
            "shell command timed out after 180s: /data/open-xiaoai/start-agent.sh stop",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
        let owned = still_owned_socket("streak-timeout");

        let code = yield_after_consecutive_failures(
            &device,
            Some(Path::new("/data/open-xiaoai/start-agent.sh")),
            &owned,
            &failures,
        )
        .await;

        assert_eq!(
            code,
            Some(1),
            "超时说明脚本可能停在还原中途，必须退出而不是假装健康"
        );
        let _ = std::fs::remove_file(owned.path());
    }

    #[tokio::test]
    async fn consecutive_failures_keep_running_only_when_the_frontend_is_untouched() {
        let shell = Arc::new(RecordingShell::exiting_with(
            LAUNCHER_EXIT_UNTOUCHED,
            "another launcher operation is in progress",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
        let owned = still_owned_socket("streak-keep");

        let code = yield_after_consecutive_failures(
            &device,
            Some(Path::new("/data/open-xiaoai/start-agent.sh")),
            &owned,
            &failures,
        )
        .await;

        assert_eq!(
            code, None,
            "前端未受影响时不该退出：让位这次没跑起来而已，麦克风还在我们手里"
        );
        assert_eq!(
            failures.load(Ordering::Relaxed),
            0,
            "计数必须清零，否则每个 tick 都会再试一次让位"
        );
        let _ = std::fs::remove_file(owned.path());
    }

    const RELOAD_YAML: &str = r#"runtime:
  session_idle_timeout_s: 42
llm:
  model: reloaded-model
device:
  tts_command: "/usr/sbin/tts_play.sh --changed {text}"
asr:
  open_ai:
    model: reloaded-asr
agent:
  tools_enabled: false
"#;

    const BROKEN_YAML: &str = "runtime: [not a mapping";

    const SOCKET_YAML: &str = r#"runtime:
  speech_socket: /tmp/other/speech.usock
  session_idle_timeout_s: 7
"#;

    fn temp_config_path(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "xiaoai-reload-{tag}-{}-{stamp}.yaml",
            std::process::id()
        ))
    }

    fn still_owned_socket(tag: &str) -> SocketOwnership {
        use std::os::unix::fs::MetadataExt;
        let path = temp_config_path(tag).with_extension("usock");
        std::fs::write(&path, b"x").expect("写临时 socket 占位文件");
        let inode = std::fs::metadata(&path).expect("读 inode").ino();
        SocketOwnership::for_test(path, inode)
    }

    fn taken_over_socket(tag: &str) -> SocketOwnership {
        use std::os::unix::fs::MetadataExt;
        let live = temp_config_path(tag).with_extension("usock");
        let native = live.with_extension("native.usock");
        std::fs::write(&live, b"agent").expect("写 agent socket 占位");
        std::fs::write(&native, b"native").expect("写原生 socket 占位");
        let mine = std::fs::metadata(&live).expect("读 inode").ino();
        std::fs::remove_file(&live).expect("rm");
        std::fs::rename(&native, &live).expect("mv");
        SocketOwnership::for_test(live, mine)
    }

    #[test]
    fn reload_gate_coalesces_requests_arriving_mid_reload() {
        let gate = ReloadGate::default();
        assert!(gate.begin(), "第一个请求拿到执行权");
        assert!(!gate.begin(), "重载进行中：本次请求被并入下一轮");
        assert!(gate.finish_round(), "有合并进来的请求，需要再跑一轮");
        assert!(!gate.finish_round(), "没有新请求就结束");
        gate.release();
        assert!(gate.begin(), "释放之后又能接新请求");
    }

    #[test]
    fn reload_guard_releases_the_gate_on_drop() {
        let gate = Arc::new(ReloadGate::default());
        assert!(gate.begin());
        {
            let _guard = ReloadGuard(gate.clone());
            assert!(!gate.begin(), "持锁期间不该再接新任务");
        }
        assert!(gate.begin(), "guard drop 之后必须已复位");
    }

    #[test]
    fn reload_guard_releases_the_gate_even_when_the_task_panics() {
        let gate = Arc::new(ReloadGate::default());
        assert!(gate.begin());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ReloadGuard(gate.clone());
            panic!("simulated reload panic");
        }));
        assert!(outcome.is_err(), "这条用例本身要制造一次 panic");
        assert!(
            gate.begin(),
            "panic 展开后 running 必须复位，否则热加载永久失效"
        );
    }

    #[tokio::test]
    async fn config_reload_applies_hot_sections_and_keeps_pending_ones() {
        let orch = test_orchestrator(Arc::new(RecordingShell::always_ok())).await;
        let before = orch.config.get().await;
        let path = temp_config_path("apply");
        std::fs::write(&path, RELOAD_YAML).unwrap();

        apply_config_reload(&orch.config, &path, &orch.agent).await;
        let _ = std::fs::remove_file(&path);

        let after = orch.config.get().await;
        assert_eq!(after.runtime.session_idle_timeout_s, 42.0);
        assert_eq!(after.llm.model, "reloaded-model");
        assert_eq!(after.device.tts_command, before.device.tts_command);
        assert_eq!(after.asr.open_ai.model, before.asr.open_ai.model);
    }

    #[tokio::test]
    async fn config_reload_keeps_old_config_when_the_file_is_broken() {
        let orch = test_orchestrator(Arc::new(RecordingShell::always_ok())).await;
        let before = orch.config.get().await;
        let path = temp_config_path("broken");
        std::fs::write(&path, BROKEN_YAML).unwrap();

        apply_config_reload(&orch.config, &path, &orch.agent).await;
        let _ = std::fs::remove_file(&path);

        let after = orch.config.get().await;
        assert_eq!(after.llm.model, before.llm.model);
        assert_eq!(
            after.runtime.session_idle_timeout_s,
            before.runtime.session_idle_timeout_s
        );
    }

    #[tokio::test]
    async fn config_reload_is_a_noop_when_only_restart_sections_changed() {
        let orch = test_orchestrator(Arc::new(RecordingShell::always_ok())).await;
        let before = orch.config.get().await;
        let path = temp_config_path("device-only");
        std::fs::write(
            &path,
            "runtime:\n  session_idle_timeout_s: 1\nagent:\n  tools_enabled: false\ndevice:\n  tts_command: \"/usr/sbin/tts_play.sh --changed {text}\"\n",
        )
        .unwrap();

        apply_config_reload(&orch.config, &path, &orch.agent).await;
        let _ = std::fs::remove_file(&path);

        let after = orch.config.get().await;
        assert_eq!(after.device.tts_command, before.device.tts_command);
        assert_eq!(
            after.runtime.session_idle_timeout_s,
            before.runtime.session_idle_timeout_s
        );
    }

    #[tokio::test]
    async fn config_reload_ignores_speech_socket_changes() {
        let orch = test_orchestrator(Arc::new(RecordingShell::always_ok())).await;
        let before = orch.config.get().await.runtime.speech_socket.clone();
        let path = temp_config_path("socket");
        std::fs::write(&path, SOCKET_YAML).unwrap();

        apply_config_reload(&orch.config, &path, &orch.agent).await;
        let _ = std::fs::remove_file(&path);

        let after = orch.config.get().await;
        assert_eq!(after.runtime.speech_socket, before);
        assert_eq!(after.runtime.session_idle_timeout_s, 7.0);
    }

    #[tokio::test]
    async fn yield_to_native_without_a_launcher_runs_nothing() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = Device::with_runner(DeviceConfig::default(), shell.clone());

        assert!(
            !yield_to_native(&device, None, &still_owned_socket("no-launcher"), "fatal")
                .await
                .succeeded()
        );

        assert_eq!(shell.command_count(), 0, "找不到 launcher 就别瞎跑命令");
        assert!(shell.detached_scripts().is_empty());
    }

    #[test]
    fn cli_parses_default_daemon_and_status_subcommand() {
        let daemon = Cli::try_parse_from(["xiaoai-agent", "-c", "/tmp/test.yaml"]).unwrap();
        assert_eq!(daemon.config, PathBuf::from("/tmp/test.yaml"));
        assert_eq!(daemon.command, None);

        let status =
            Cli::try_parse_from(["xiaoai-agent", "status", "-c", "/tmp/test.yaml"]).unwrap();
        assert_eq!(status.config, PathBuf::from("/tmp/test.yaml"));
        assert_eq!(status.command, Some(CliCommand::Status));
    }

    #[tokio::test]
    async fn run_status_command_succeeds_for_valid_config() {
        let path = temp_config_path("status-cmd");
        std::fs::write(
            &path,
            "runtime:\n  mode: exclusive\nllm:\n  base_url: http://127.0.0.1:1/v1\nasr:\n  open_ai:\n    base_url: http://127.0.0.1:1/v1\n",
        )
        .unwrap();
        let res = run_status_command(&path).await;
        let _ = std::fs::remove_file(&path);
        assert!(res.is_ok(), "status subcommand failed: {res:?}");
    }

    #[tokio::test]
    async fn drain_stale_followup_turns_rejects_queued_turns_before_new_request() {
        let (stale_turn, _input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(91);
        let (tx, mut rx) = mpsc::channel(2);
        tx.send(stale_turn).await.unwrap();

        drain_stale_followup_turns(&mut rx, &handle).await;

        assert!(rx.try_recv().is_err());
        assert_eq!(
            commands(&recorder).await,
            vec![
                TestCommand::StopCapture,
                TestCommand::Finish {
                    continue_dialog: false
                }
            ]
        );
    }

    #[tokio::test]
    async fn exclusive_music_voice_control_stops_playing_song_locally() {
        let shell = Arc::new(RecordingShell::always_ok());
        let orch = test_orchestrator(shell.clone()).await;
        let config = orch.config.get().await;
        orch.audio
            .music
            .set_current_for_test("kw_1", "晴天", "https://example.com/1.mp3")
            .await;
        assert!(orch.audio.music.interrupt_for_wake().await);

        let mut empty_count = 0u32;
        let mut ctx = LocalIntentContext {
            is_followup: false,
            answered_turns: 0,
            consecutive_empty_or_noise: &mut empty_count,
            max_empty_retries: 2,
            allow_wake_ack: true,
            enter_prompt: WAKE_ACK_PROMPT,
            exit_prompt: EXIT_FAREWELL_PROMPT,
            prefer_exit_earcon: false,
            speak_exit_on_noise_exhausted: false,
        };
        let outcome =
            handle_local_intent("停止播放音乐", &config, &orch.audio, &orch.agent, &mut ctx).await;
        assert_eq!(outcome, LocalIntentOutcome::EndSession);
        assert!(!orch.audio.music.has_current().await);
        let before_resume = shell.count_matching("mphelper play");
        orch.audio.music.restore_after_interruption().await;
        assert_eq!(shell.count_matching("mphelper play"), before_resume);
    }

    #[tokio::test]
    async fn in_flight_turn_is_recorded_as_partial_history_when_interrupted_by_wake() {
        let orch = test_orchestrator(Arc::new(RecordingShell::always_ok())).await;
        orch.in_flight.begin("北京今天天气怎么样");
        orch.in_flight.append("今天北京晴，最高气温二十五度。");

        let (user_q, partial_ans) = orch.in_flight.take().expect("必须拿到被打断轮次的在途问答");
        orch.agent.record_partial_turn(&user_q, &partial_ans).await;
        orch.agent.reset_session("wake keyword").await;

        assert_eq!(
            orch.agent.history_len_for_test().await,
            1,
            "唤醒打断后半截问答必须保留在近期历史中，供用户纠错追问"
        );
    }

    #[test]
    fn cli_parses_doctor_subcommand() {
        let doctor =
            Cli::try_parse_from(["xiaoai-agent", "doctor", "-c", "/tmp/test.yaml"]).unwrap();
        assert_eq!(doctor.config, PathBuf::from("/tmp/test.yaml"));
        assert_eq!(doctor.command, Some(CliCommand::Doctor { probe_mic: false }));

        let probe = Cli::try_parse_from(["xiaoai-agent", "doctor", "--probe-mic"]).unwrap();
        assert_eq!(probe.command, Some(CliCommand::Doctor { probe_mic: true }));
    }

    #[tokio::test]
    async fn run_doctor_command_reports_for_valid_config_and_fails_for_missing() {
        let path = temp_config_path("doctor-cmd");
        std::fs::write(
            &path,
            "runtime:\n  mode: exclusive\nllm:\n  base_url: http://127.0.0.1:1/v1\nasr:\n  open_ai:\n    base_url: http://127.0.0.1:1/v1\n",
        )
        .unwrap();
        let res = run_doctor_command(&path, false).await;
        let _ = std::fs::remove_file(&path);
        assert!(res.is_ok(), "doctor failed: {res:?}");

        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let kws_addr = server.local_addr().unwrap();
        let srv_task = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            if let Ok((_, peer)) = server.recv_from(&mut buf).await {
                let _ = server.send_to(b"XPONG\n", peer).await;
            }
        });
        let dual_path = temp_config_path("doctor-dual");
        std::fs::write(
            &dual_path,
            format!(
                "runtime:\n  mode: dual\ndual:\n  kws:\n    endpoint: {kws_addr}\nllm:\n  base_url: http://127.0.0.1:1/v1\nasr:\n  open_ai:\n    base_url: http://127.0.0.1:1/v1\n"
            ),
        )
        .unwrap();
        let dual_res = run_doctor_command(&dual_path, false).await;
        let _ = std::fs::remove_file(&dual_path);
        srv_task.await.unwrap();
        assert!(dual_res.is_ok(), "doctor in dual mode failed: {dual_res:?}");

        let missing = run_doctor_command(Path::new("/nonexistent/xiaoai-agent.yaml"), false).await;
        assert!(missing.is_err());
    }

    #[tokio::test]
    async fn control_runner_clear_history_and_stop_work_without_a_session() {
        let orch = test_orchestrator(Arc::new(RecordingShell::always_ok())).await;
        let (tx, mut rx) = mpsc::channel(1);
        let mut runner = ControlRunner::new(
            orch.config.clone(),
            orch.agent.clone(),
            orch.audio.clone(),
            tx,
        );
        orch.agent.record_partial_turn("q", "a").await;
        assert_eq!(orch.agent.history_len_for_test().await, 1);
        runner.handle(ControlCommand::ClearHistory).await;
        assert_eq!(orch.agent.history_len_for_test().await, 0);

        runner.handle(ControlCommand::Stop).await;
        assert!(runner.task.is_none());

        runner.handle(ControlCommand::Reload).await;
        assert!(rx.try_recv().is_ok(), "Reload 必须触发 reload_tx");
    }
}

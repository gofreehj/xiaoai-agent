//! Shared half-duplex real-time dialogue session runner with 4-channel barge-in
//! and false-exit protection.
//!
//! Coordinates [`RealtimeDriver`], [`ExternalKwsHandle`], [`AudioCoordinator`],
//! [`AgentRuntime`], and [`NativeWakeMonitor`]:
//! - Replays 2.56s pre-wake PCM via `ExternalKwsHandle::start_realtime_stream()`
//! - Enforces half-duplex uplink gating during `Speaking` and `Tail` (and whenever `sys_active` is set)
//! - Probes downlink audio (`S16_LE` vs `F32_LE`, rejects `OggS`) before feeding `AplayStreamPlayer`
//! - Protects against self-exit with `ExitEchoGuard`, `exit_playback_threshold`, and `classify_exit_command`
//! - Supports 4-channel barge-in: native XiaoAI, external wake, external exit, and reserved `stop_key`

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::agent::AgentRuntime;
use crate::audio::output_format::OutputFormatProbe;
use crate::audio::stream_player::{make_session_cue_pcm, CueKind, StreamAudioSpec};
use crate::audio::AudioCoordinator;
use crate::config::{AppConfig, RealtimeFallback};
use crate::dual::is_native_xiaoai_utterance;
use crate::frontend::external_kws::{AudioChunk, BargeInEvent, ExternalKwsHandle};
use crate::frontend::native_monitor::{wait_native_wake, NativeWakeMonitor};
use crate::frontend::BargeInSignal;
use crate::realtime::daily_usage::DailyUsageGuard;
use crate::realtime::smart_turn::{conversation_words, looks_unfinished, should_commit_turn};
use crate::realtime::{RealtimeDriver, RealtimeEvent};
use crate::runtime::telemetry::AgentPhase;
use crate::session::{play_optional_earcon, speak_prompt_wait};
use crate::tts::{
    classify_exit_command, classify_music_voice_control, strip_wake_prefix, ExitAction,
    ExitEchoGuard, MusicVoiceControl,
};

const DRAIN_TIMEOUT: Duration = Duration::from_secs(12);

/// Outcome returned when a [`RealtimeSessionRunner`] session ends.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeRunOutcome {
    Completed,
    NativeYield,
    FallbackToCascade { initial_query: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunnerPhase {
    Listening,
    Speaking,
    Tail,
}

enum ExitReason {
    Outcome(RealtimeRunOutcome),
    Error(anyhow::Error),
}

/// Shared half-duplex real-time voice session runner.
#[allow(dead_code)]
pub struct RealtimeSessionRunner;

#[allow(dead_code)]
impl RealtimeSessionRunner {
    /// Run a real-time session from a driver connection attempt (`Result<D>`),
    /// enforcing daily token quota and applying `config.realtime.fallback` on error.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_driver_result<D: RealtimeDriver>(
        driver_res: anyhow::Result<D>,
        config: &AppConfig,
        audio: &AudioCoordinator,
        agent: &Arc<AgentRuntime>,
        kws: &mut ExternalKwsHandle,
        native_monitor: &NativeWakeMonitor,
        native_rx: &mut broadcast::Receiver<()>,
        stop_key_rx: Option<&mut mpsc::Receiver<()>>,
    ) -> anyhow::Result<RealtimeRunOutcome> {
        let usage_guard = DailyUsageGuard::from_config(&config.realtime.daily_usage);
        if usage_guard.is_limit_exceeded() {
            if let Ok(mut driver) = driver_res {
                let _ = driver.close().await;
            }
            warn!(
                limit = usage_guard.daily_token_limit(),
                used = usage_guard.read_today(),
                "Realtime daily token limit exceeded before session start"
            );
            return Self::handle_connect_failure(
                config,
                audio,
                native_monitor,
                anyhow::anyhow!("daily realtime token limit exceeded"),
            )
            .await;
        }
        let driver = match driver_res {
            Ok(d) => d,
            Err(err) => {
                warn!(error = %err, fallback = ?config.realtime.fallback, "Realtime driver connection failed");
                return Self::handle_connect_failure(config, audio, native_monitor, err).await;
            }
        };
        Self::run_with_driver(
            driver,
            config,
            audio,
            agent,
            kws,
            native_monitor,
            native_rx,
            stop_key_rx,
        )
        .await
    }

    async fn handle_connect_failure(
        config: &AppConfig,
        audio: &AudioCoordinator,
        native_monitor: &NativeWakeMonitor,
        err: anyhow::Error,
    ) -> anyhow::Result<RealtimeRunOutcome> {
        match config.realtime.fallback {
            RealtimeFallback::Cascade => {
                Ok(RealtimeRunOutcome::FallbackToCascade { initial_query: None })
            }
            RealtimeFallback::NativeAlert => {
                if !native_monitor.is_blocked() {
                    audio.set_phase(AgentPhase::Speaking).await;
                    speak_prompt_wait(audio, config, "抱歉，实时语音服务连接失败").await;
                }
                Ok(RealtimeRunOutcome::Completed)
            }
            RealtimeFallback::None => Err(err),
        }
    }

    /// Run a multi-turn half-duplex real-time dialogue session on an established `driver`.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_driver<D: RealtimeDriver>(
        mut driver: D,
        config: &AppConfig,
        audio: &AudioCoordinator,
        agent: &Arc<AgentRuntime>,
        kws: &mut ExternalKwsHandle,
        native_monitor: &NativeWakeMonitor,
        native_rx: &mut broadcast::Receiver<()>,
        mut stop_key_rx: Option<&mut mpsc::Receiver<()>>,
    ) -> anyhow::Result<RealtimeRunOutcome> {
        if native_monitor.is_blocked() {
            let _ = driver.close().await;
            return Ok(RealtimeRunOutcome::NativeYield);
        }
        let usage_guard = DailyUsageGuard::from_config(&config.realtime.daily_usage);
        if usage_guard.is_limit_exceeded() {
            let _ = driver.close().await;
            warn!(
                limit = usage_guard.daily_token_limit(),
                used = usage_guard.read_today(),
                "Realtime daily token limit exceeded before session start"
            );
            return Self::handle_connect_failure(
                config,
                audio,
                native_monitor,
                anyhow::anyhow!("daily realtime token limit exceeded"),
            )
            .await;
        }

        let mut audio_rx = kws.start_realtime_stream().await?;
        let base_spec = audio.stream_player.default_spec().await;
        let stream_spec = StreamAudioSpec {
            aplay_path: base_spec.aplay_path,
            device: if base_spec.command_override.is_some() {
                base_spec.device
            } else {
                config.realtime.output_device.clone()
            },
            sample_rate: driver.output_sample_rate().max(8000),
            channels: 1,
            playback_gain: config.realtime.playback_gain,
            command_override: base_spec.command_override,
        };

        let tail_duration = Duration::from_millis(config.realtime.playback_tail_ms);
        let idle_duration = Duration::from_secs(config.realtime.idle_timeout_s.max(5));
        let exit_threshold = config.dual.capture.exit_playback_threshold;

        let mut phase = RunnerPhase::Listening;
        let mut output_probe = OutputFormatProbe::new();
        let mut exit_echo_guard = ExitEchoGuard::new();
        let mut player_gen: Option<u64> = None;

        let mut turn_seq: u64 = 1;
        let mut answered_turns: u32 = 0;
        let mut session_ready = false;
        let mut pending_asr = String::new();
        let mut active_user_text = String::new();
        let mut current_assistant_text = String::new();

        let far_future = Instant::now() + Duration::from_secs(86_400);
        let tail_timer = tokio::time::sleep_until(far_future);
        tokio::pin!(tail_timer);
        let idle_timer = tokio::time::sleep_until(Instant::now() + idle_duration);
        tokio::pin!(idle_timer);
        let smart_turn_cfg = &config.realtime.smart_turn;
        let smart_turn_timer = tokio::time::sleep_until(far_future);
        tokio::pin!(smart_turn_timer);
        let mut asr_quiet_since: Option<Instant> = None;
        let mut smart_turn_committed = false;

        audio.set_phase(AgentPhase::Listening).await;

        let exit_reason = loop {
            let tail_armed = phase == RunnerPhase::Tail;
            let idle_armed = phase == RunnerPhase::Listening;
            let smart_turn_armed = smart_turn_cfg.enabled
                && phase == RunnerPhase::Listening
                && !smart_turn_committed
                && asr_quiet_since.is_some();

            tokio::select! {
                _ = wait_native_wake(native_rx) => {
                    info!("Realtime 会话检测到原生小爱唤醒，立即停止播报并静默让位");
                    audio.stream_player.interrupt().await;
                    let _ = driver.cancel_response().await;
                    let _ = driver.close().await;
                    break ExitReason::Outcome(RealtimeRunOutcome::NativeYield);
                }

                barge_res = kws.next_barge_in_event() => {
                    match barge_res {
                        Ok(ev) => {
                            if ev.signal == BargeInSignal::Wake {
                                asr_quiet_since = None;
                                smart_turn_committed = false;
                                smart_turn_timer.as_mut().reset(far_future);
                            }
                            if let Some(outcome) = Self::handle_barge_in(
                                ev,
                                &mut phase,
                                &mut driver,
                                config,
                                audio,
                                agent,
                                native_monitor,
                                exit_threshold,
                                &mut exit_echo_guard,
                                &mut player_gen,
                                &mut active_user_text,
                                &mut current_assistant_text,
                                &mut pending_asr,
                                &mut turn_seq,
                                tail_timer.as_mut(),
                                idle_timer.as_mut(),
                                far_future,
                                idle_duration,
                            ).await? {
                                break ExitReason::Outcome(outcome);
                            }
                        }
                        Err(err) => {
                            audio.stream_player.interrupt().await;
                            let _ = driver.close().await;
                            break ExitReason::Error(err);
                        }
                    }
                }

                Some(()) = recv_stop_key(&mut stop_key_rx) => {
                    if matches!(phase, RunnerPhase::Speaking | RunnerPhase::Tail) {
                        info!("实体按键触发打断，停止当前播报并切回 Listening");
                        audio.stream_player.interrupt().await;
                        player_gen = None;
                        let _ = driver.cancel_response().await;
                        if !active_user_text.is_empty() || !current_assistant_text.is_empty() {
                            agent.record_external_turn(&active_user_text, &current_assistant_text, true).await;
                            active_user_text.clear();
                            current_assistant_text.clear();
                        }
                        pending_asr.clear();
                        asr_quiet_since = None;
                        smart_turn_committed = false;
                        smart_turn_timer.as_mut().reset(far_future);
                        exit_echo_guard.reset();
                        turn_seq += 1;
                        phase = RunnerPhase::Listening;
                        tail_timer.as_mut().reset(far_future);
                        idle_timer.as_mut().reset(Instant::now() + idle_duration);
                        audio.set_phase(AgentPhase::Listening).await;
                    }
                }

                maybe_chunk = audio_rx.recv() => {
                    let Some(chunk) = maybe_chunk else {
                        let _ = driver.close().await;
                        break ExitReason::Outcome(RealtimeRunOutcome::Completed);
                    };
                    if let Err(err) = Self::handle_audio_chunk(phase, &mut driver, chunk).await {
                        audio.stream_player.interrupt().await;
                        let _ = driver.close().await;
                        break ExitReason::Error(err);
                    }
                }

                ev_res = driver.next_event() => {
                    match ev_res {
                        Ok(Some(ev)) => {
                            if let Some(outcome) = Self::handle_driver_event(
                                ev,
                                &mut phase,
                                &mut driver,
                                config,
                                audio,
                                agent,
                                native_monitor,
                                &usage_guard,
                                &stream_spec,
                                &mut output_probe,
                                &mut exit_echo_guard,
                                &mut player_gen,
                                &mut turn_seq,
                                &mut answered_turns,
                                &mut session_ready,
                                &mut pending_asr,
                                &mut active_user_text,
                                &mut current_assistant_text,
                                &mut asr_quiet_since,
                                &mut smart_turn_committed,
                                smart_turn_timer.as_mut(),
                                tail_timer.as_mut(),
                                idle_timer.as_mut(),
                                tail_duration,
                                idle_duration,
                                far_future,
                            ).await? {
                                break ExitReason::Outcome(outcome);
                            }
                        }
                        Ok(None) => {
                            if let Some(gen) = player_gen.take() {
                                let _ = audio.stream_player.finish_and_drain(gen, DRAIN_TIMEOUT).await;
                            }
                            if !session_ready
                                && answered_turns == 0
                                && config.realtime.fallback == RealtimeFallback::Cascade
                            {
                                let query = first_non_empty(&active_user_text, &pending_asr);
                                break ExitReason::Outcome(RealtimeRunOutcome::FallbackToCascade {
                                    initial_query: query,
                                });
                            }
                            break ExitReason::Outcome(RealtimeRunOutcome::Completed);
                        }
                        Err(err) => {
                            audio.stream_player.interrupt().await;
                            let _ = driver.close().await;
                            if answered_turns == 0 && config.realtime.fallback == RealtimeFallback::Cascade {
                                let query = first_non_empty(&active_user_text, &pending_asr);
                                warn!(error = %err, "Realtime turn 0 failed; falling back to cascade");
                                break ExitReason::Outcome(RealtimeRunOutcome::FallbackToCascade {
                                    initial_query: query,
                                });
                            }
                            break ExitReason::Error(err);
                        }
                    }
                }

                _ = &mut smart_turn_timer, if smart_turn_armed => {
                    let quiet_ms = asr_quiet_since
                        .map(|s| s.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    if should_commit_turn(
                        &pending_asr,
                        quiet_ms,
                        smart_turn_cfg.complete_quiet_ms,
                        smart_turn_cfg.unfinished_quiet_ms,
                    ) {
                        smart_turn_committed = true;
                        asr_quiet_since = None;
                        smart_turn_timer.as_mut().reset(far_future);
                        debug!(
                            text = %pending_asr,
                            quiet_ms,
                            "Smart Turn triggered commit_user_turn"
                        );
                        if let Err(err) = driver.commit_user_turn().await {
                            warn!(error = %err, "Smart Turn commit_user_turn failed");
                        }
                    } else {
                        smart_turn_timer.as_mut().reset(far_future);
                    }
                }

                _ = &mut tail_timer, if tail_armed => {
                    exit_echo_guard.reset();
                    phase = RunnerPhase::Listening;
                    tail_timer.as_mut().reset(far_future);
                    idle_timer.as_mut().reset(Instant::now() + idle_duration);
                    audio.set_phase(AgentPhase::Listening).await;
                }

                _ = &mut idle_timer, if idle_armed => {
                    info!(idle_s = idle_duration.as_secs(), "Realtime 会话空闲超时，优雅退出");
                    let _ = driver.close().await;
                    if !native_monitor.is_blocked() {
                        Self::play_exit_feedback(audio, config, ExitAction::Farewell).await;
                    }
                    break ExitReason::Outcome(RealtimeRunOutcome::Completed);
                }
            }
        };

        // Guaranteed cleanup before returning.
        kws.stop_stream().await;
        match exit_reason {
            ExitReason::Outcome(outcome) => Ok(outcome),
            ExitReason::Error(err) => {
                audio.stream_player.interrupt().await;
                Err(err)
            }
        }
    }

    async fn handle_audio_chunk<D: RealtimeDriver>(
        phase: RunnerPhase,
        driver: &mut D,
        chunk: AudioChunk,
    ) -> anyhow::Result<()> {
        if phase != RunnerPhase::Listening || chunk.sys_active || chunk.pcm.is_empty() {
            return Ok(());
        }
        driver.send_audio(&chunk.pcm).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_barge_in<D: RealtimeDriver>(
        ev: BargeInEvent,
        phase: &mut RunnerPhase,
        driver: &mut D,
        config: &AppConfig,
        audio: &AudioCoordinator,
        agent: &Arc<AgentRuntime>,
        native_monitor: &NativeWakeMonitor,
        exit_threshold: f32,
        exit_echo_guard: &mut ExitEchoGuard,
        player_gen: &mut Option<u64>,
        active_user_text: &mut String,
        current_assistant_text: &mut String,
        pending_asr: &mut String,
        turn_seq: &mut u64,
        mut tail_timer: std::pin::Pin<&mut tokio::time::Sleep>,
        mut idle_timer: std::pin::Pin<&mut tokio::time::Sleep>,
        far_future: Instant,
        idle_duration: Duration,
    ) -> anyhow::Result<Option<RealtimeRunOutcome>> {
        match ev.signal {
            BargeInSignal::Wake => {
                info!(keyword = %ev.keyword, "Realtime 会话收到唤醒词打断，取消旧回复并切回聆听");
                audio.stream_player.interrupt().await;
                *player_gen = None;
                let _ = driver.cancel_response().await;
                if !active_user_text.is_empty() || !current_assistant_text.is_empty() {
                    agent
                        .record_external_turn(active_user_text, current_assistant_text, true)
                        .await;
                    active_user_text.clear();
                    current_assistant_text.clear();
                }
                pending_asr.clear();
                exit_echo_guard.reset();
                *turn_seq += 1;
                *phase = RunnerPhase::Listening;
                tail_timer.as_mut().reset(far_future);
                idle_timer.as_mut().reset(Instant::now() + idle_duration);
                audio.set_phase(AgentPhase::Listening).await;
                Ok(None)
            }
            BargeInSignal::ExitCmd => {
                let in_playback_or_tail = matches!(*phase, RunnerPhase::Speaking | RunnerPhase::Tail);
                if in_playback_or_tail {
                    if let Some(score) = ev.score {
                        if score < exit_threshold {
                            debug!(
                                keyword = %ev.keyword,
                                score,
                                threshold = exit_threshold,
                                "ignored low-score exit command during playback/tail"
                            );
                            return Ok(None);
                        }
                    }
                    if exit_echo_guard.contains(&ev.keyword) {
                        info!(
                            keyword = %ev.keyword,
                            "ExitEchoGuard blocked self-echo exit command during playback/tail"
                        );
                        return Ok(None);
                    }
                }
                info!(keyword = %ev.keyword, ?ev.score, "Realtime 会话收到有效退出指令，结束会话");
                audio.stream_player.interrupt().await;
                *player_gen = None;
                let _ = driver.cancel_response().await;
                let _ = driver.close().await;
                if !active_user_text.is_empty() || !current_assistant_text.is_empty() {
                    agent
                        .record_external_turn(active_user_text, current_assistant_text, true)
                        .await;
                }
                if !native_monitor.is_blocked() {
                    Self::play_exit_feedback(audio, config, ExitAction::Farewell).await;
                }
                agent.reset_session("user exit command").await;
                Ok(Some(RealtimeRunOutcome::Completed))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_driver_event<D: RealtimeDriver>(
        ev: RealtimeEvent,
        phase: &mut RunnerPhase,
        driver: &mut D,
        config: &AppConfig,
        audio: &AudioCoordinator,
        agent: &Arc<AgentRuntime>,
        native_monitor: &NativeWakeMonitor,
        usage_guard: &DailyUsageGuard,
        stream_spec: &StreamAudioSpec,
        output_probe: &mut OutputFormatProbe,
        exit_echo_guard: &mut ExitEchoGuard,
        player_gen: &mut Option<u64>,
        turn_seq: &mut u64,
        answered_turns: &mut u32,
        session_ready: &mut bool,
        pending_asr: &mut String,
        active_user_text: &mut String,
        current_assistant_text: &mut String,
        asr_quiet_since: &mut Option<Instant>,
        smart_turn_committed: &mut bool,
        mut smart_turn_timer: std::pin::Pin<&mut tokio::time::Sleep>,
        mut tail_timer: std::pin::Pin<&mut tokio::time::Sleep>,
        mut idle_timer: std::pin::Pin<&mut tokio::time::Sleep>,
        tail_duration: Duration,
        idle_duration: Duration,
        far_future: Instant,
    ) -> anyhow::Result<Option<RealtimeRunOutcome>> {
        match ev {
            RealtimeEvent::SessionReady { session_id } => {
                debug!(session_id = %session_id, "Realtime session ready");
                *session_ready = true;
                if *phase == RunnerPhase::Listening {
                    idle_timer.as_mut().reset(Instant::now() + idle_duration);
                }
            }
            RealtimeEvent::UserSpeechStarted => {
                pending_asr.clear();
                *asr_quiet_since = None;
                *smart_turn_committed = false;
                smart_turn_timer.as_mut().reset(far_future);
                if *phase == RunnerPhase::Listening {
                    idle_timer.as_mut().reset(Instant::now() + idle_duration);
                    audio.set_phase(AgentPhase::UserSpeaking).await;
                }
            }
            RealtimeEvent::UserTranscriptDelta(text) => {
                if text != *pending_asr {
                    *pending_asr = text;
                    *smart_turn_committed = false;
                    if config.realtime.smart_turn.enabled
                        && *phase == RunnerPhase::Listening
                        && !conversation_words(pending_asr).is_empty()
                    {
                        let now = Instant::now();
                        *asr_quiet_since = Some(now);
                        let quiet_ms = if looks_unfinished(pending_asr) {
                            config.realtime.smart_turn.unfinished_quiet_ms
                        } else {
                            config.realtime.smart_turn.complete_quiet_ms
                        };
                        smart_turn_timer
                            .as_mut()
                            .reset(now + Duration::from_millis(quiet_ms));
                    } else {
                        *asr_quiet_since = None;
                        smart_turn_timer.as_mut().reset(far_future);
                    }
                }
                if *phase == RunnerPhase::Listening {
                    idle_timer.as_mut().reset(Instant::now() + idle_duration);
                }
            }
            RealtimeEvent::UserTranscriptCompleted(raw_text) => {
                pending_asr.clear();
                *asr_quiet_since = None;
                *smart_turn_committed = true;
                smart_turn_timer.as_mut().reset(far_future);
                *active_user_text = raw_text.clone();
                if *phase == RunnerPhase::Listening {
                    idle_timer.as_mut().reset(Instant::now() + idle_duration);
                }

                if is_native_xiaoai_utterance(&raw_text) {
                    info!(raw = %raw_text, "Realtime ASR 检测到原生小爱唤醒词，静默让位");
                    audio.stream_player.interrupt().await;
                    *player_gen = None;
                    let _ = driver.cancel_response().await;
                    let _ = driver.close().await;
                    native_monitor.notify_native_utterance();
                    return Ok(Some(RealtimeRunOutcome::NativeYield));
                }

                let cleaned = strip_wake_prefix(&raw_text);
                if let Some(action) = crate::session::intent::classify_volume_command(&cleaned) {
                    info!(?action, "Realtime 本地快捷控制：调节音量");
                    if let Err(err) = audio.execute_volume(action).await {
                        warn!(?action, error = %err, "Realtime 本地音量控制失败");
                    }
                    return Ok(None);
                }

                if audio.music.has_current().await {
                    if let Some(ctrl) = classify_music_voice_control(&cleaned) {
                        info!(?ctrl, "Realtime 本地快捷控制：音乐控制");
                        let should_exit = match ctrl {
                            MusicVoiceControl::Stop => {
                                let _ = audio.music.stop().await;
                                true
                            }
                            MusicVoiceControl::Pause => {
                                let _ = audio.music.pause().await;
                                true
                            }
                            MusicVoiceControl::Resume => {
                                let _ = audio.music.resume().await;
                                false
                            }
                            MusicVoiceControl::Next => {
                                let _ = audio.music.next().await;
                                false
                            }
                            MusicVoiceControl::Previous => {
                                let _ = audio.music.previous().await;
                                false
                            }
                        };
                        if should_exit {
                            audio.stream_player.interrupt().await;
                            *player_gen = None;
                            let _ = driver.cancel_response().await;
                            let _ = driver.close().await;
                            return Ok(Some(RealtimeRunOutcome::Completed));
                        }
                        return Ok(None);
                    }
                }

                if let Some(action) =
                    classify_exit_command(&cleaned, &config.dual.exit_keywords)
                {
                    info!(raw = %raw_text, ?action, "Realtime ASR 识别到退出指令，结束会话");
                    audio.stream_player.interrupt().await;
                    *player_gen = None;
                    let _ = driver.cancel_response().await;
                    let _ = driver.close().await;
                    if !native_monitor.is_blocked() {
                        Self::play_exit_feedback(audio, config, action).await;
                    }
                    agent.reset_session("user exit command").await;
                    return Ok(Some(RealtimeRunOutcome::Completed));
                }
            }
            RealtimeEvent::AssistantTextDelta(delta) => {
                current_assistant_text.push_str(&delta);
                let resp_id = format!("turn-{turn_seq}");
                exit_echo_guard.update(&resp_id, &delta, false, false);
            }
            RealtimeEvent::AssistantAudioDelta(raw_pcm) => {
                if *phase != RunnerPhase::Speaking {
                    *asr_quiet_since = None;
                    *smart_turn_committed = false;
                    smart_turn_timer.as_mut().reset(far_future);
                    *phase = RunnerPhase::Speaking;
                    tail_timer.as_mut().reset(far_future);
                    output_probe.reset();
                    let gen = audio
                        .stream_player
                        .start_turn(Some(stream_spec.clone()))
                        .await;
                    *player_gen = Some(gen);
                    audio.set_phase(AgentPhase::Speaking).await;
                }
                let converted = match output_probe.inspect_and_convert(&raw_pcm) {
                    Ok(pcm) => pcm,
                    Err(err) => {
                        audio.stream_player.interrupt().await;
                        *player_gen = None;
                        let _ = driver.cancel_response().await;
                        let _ = driver.close().await;
                        return Err(err);
                    }
                };
                if let Some(gen) = *player_gen {
                    let _ = audio.stream_player.push_pcm(gen, converted).await?;
                }
            }
            RealtimeEvent::AssistantAudioDone => {
                if let Some(gen) = player_gen.take() {
                    let _ = audio.stream_player.finish_and_drain(gen, DRAIN_TIMEOUT).await;
                }
                if tail_duration.is_zero() {
                    exit_echo_guard.reset();
                    *phase = RunnerPhase::Listening;
                    idle_timer.as_mut().reset(Instant::now() + idle_duration);
                    audio.set_phase(AgentPhase::Listening).await;
                } else {
                    *phase = RunnerPhase::Tail;
                    tail_timer.as_mut().reset(Instant::now() + tail_duration);
                }
            }
            RealtimeEvent::ResponseDone { usage } => {
                let turn_tokens = usage.total_tokens();
                let today_tokens = if turn_tokens > 0 {
                    usage_guard.add_today(turn_tokens)
                } else {
                    usage_guard.read_today()
                };
                debug!(
                    total_tokens = turn_tokens,
                    today_tokens,
                    "Realtime turn completed"
                );
                if !active_user_text.is_empty() || !current_assistant_text.is_empty() {
                    agent
                        .record_external_turn(active_user_text, current_assistant_text, false)
                        .await;
                    active_user_text.clear();
                    current_assistant_text.clear();
                }
                *answered_turns += 1;
                *turn_seq += 1;
                if *phase == RunnerPhase::Listening {
                    idle_timer.as_mut().reset(Instant::now() + idle_duration);
                }
            }
            RealtimeEvent::ToolCallRequested { call_id, .. } => {
                let _ = driver.submit_tool_output(&call_id, "").await;
            }
            RealtimeEvent::Error(msg) => {
                audio.stream_player.interrupt().await;
                *player_gen = None;
                let _ = driver.close().await;
                if *answered_turns == 0 && config.realtime.fallback == RealtimeFallback::Cascade {
                    let query = first_non_empty(active_user_text, pending_asr);
                    warn!(error = %msg, "Realtime session error before first turn; falling back to cascade");
                    return Ok(Some(RealtimeRunOutcome::FallbackToCascade {
                        initial_query: query,
                    }));
                }
                anyhow::bail!("Realtime session error: {msg}");
            }
            RealtimeEvent::Closed => {
                if let Some(gen) = player_gen.take() {
                    let _ = audio.stream_player.finish_and_drain(gen, DRAIN_TIMEOUT).await;
                }
                if !*session_ready
                    && *answered_turns == 0
                    && config.realtime.fallback == RealtimeFallback::Cascade
                {
                    let query = first_non_empty(active_user_text, pending_asr);
                    return Ok(Some(RealtimeRunOutcome::FallbackToCascade {
                        initial_query: query,
                    }));
                }
                return Ok(Some(RealtimeRunOutcome::Completed));
            }
        }
        Ok(None)
    }

    async fn play_exit_feedback(audio: &AudioCoordinator, config: &AppConfig, action: ExitAction) {
        if action == ExitAction::Silent {
            return;
        }
        audio.set_phase(AgentPhase::Speaking).await;
        if play_optional_earcon(audio, config.dual.exit_earcon.as_deref()).await {
            return;
        }
        let cue = make_session_cue_pcm(CueKind::Exit, config.realtime.playback_gain);
        if !cue.is_empty() {
            let base = audio.stream_player.default_spec().await;
            let spec = StreamAudioSpec {
                sample_rate: 24_000,
                channels: 1,
                playback_gain: config.realtime.playback_gain,
                device: if base.command_override.is_some() {
                    base.device
                } else {
                    config.realtime.output_device.clone()
                },
                aplay_path: base.aplay_path,
                command_override: base.command_override,
            };
            let gen = audio.stream_player.start_turn(Some(spec)).await;
            if audio.stream_player.push_pcm(gen, cue).await.unwrap_or(false) {
                let _ = audio
                    .stream_player
                    .finish_and_drain(gen, Duration::from_secs(2))
                    .await;
                return;
            }
        }
        speak_prompt_wait(audio, config, &config.dual.exit_prompt).await;
    }
}

fn first_non_empty(primary: &str, secondary: &str) -> Option<String> {
    let p = primary.trim();
    if !p.is_empty() {
        return Some(p.to_string());
    }
    let s = secondary.trim();
    if !s.is_empty() {
        return Some(s.to_string());
    }
    None
}

async fn recv_stop_key(rx: &mut Option<&mut mpsc::Receiver<()>>) -> Option<()> {
    match rx.as_deref_mut() {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use bytes::Bytes;
    use tokio::time::{sleep, timeout};

    use super::*;
    use crate::airplay::AirPlayService;
    use crate::audio::stream_player::AplayStreamPlayer;
    use crate::config::ConfigHandle;
    use crate::device::Device;
    use crate::music::MusicService;
    use crate::realtime::{BoxFuture, RealtimeUsage};
    use crate::runtime::telemetry::RuntimeTelemetry;
    use crate::shell::RecordingShell;
    use crate::tts::{PlaybackGate, TtsHealth, TtsRouter};

    struct MockDriverState {
        sent_audio: Vec<Bytes>,
        canceled_count: usize,
        committed_count: usize,
        closed: bool,
    }

    struct MockRealtimeDriver {
        events: mpsc::Receiver<anyhow::Result<RealtimeEvent>>,
        state: Arc<Mutex<MockDriverState>>,
        auto_close_on_eof: bool,
    }

    impl MockRealtimeDriver {
        fn new(events: mpsc::Receiver<anyhow::Result<RealtimeEvent>>) -> (Self, Arc<Mutex<MockDriverState>>) {
            let state = Arc::new(Mutex::new(MockDriverState {
                sent_audio: Vec::new(),
                canceled_count: 0,
                committed_count: 0,
                closed: false,
            }));
            (
                Self {
                    events,
                    state: state.clone(),
                    auto_close_on_eof: true,
                },
                state,
            )
        }
    }

    impl RealtimeDriver for MockRealtimeDriver {
        fn output_sample_rate(&self) -> u32 {
            24_000
        }

        fn input_chunk_bytes(&self) -> usize {
            640
        }

        fn send_audio<'a>(&'a mut self, pcm: &'a [u8]) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.state
                    .lock()
                    .unwrap()
                    .sent_audio
                    .push(Bytes::copy_from_slice(pcm));
                Ok(())
            })
        }

        fn commit_user_turn<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.state.lock().unwrap().committed_count += 1;
                Ok(())
            })
        }

        fn cancel_response<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.state.lock().unwrap().canceled_count += 1;
                Ok(())
            })
        }

        fn submit_tool_output<'a>(
            &'a mut self,
            _call_id: &'a str,
            _output: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move { Ok(()) })
        }

        fn next_event<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<Option<RealtimeEvent>>> {
            Box::pin(async move {
                match self.events.recv().await {
                    Some(Ok(ev)) => Ok(Some(ev)),
                    Some(Err(err)) => Err(err),
                    None if self.auto_close_on_eof => Ok(None),
                    None => std::future::pending().await,
                }
            })
        }

        fn close<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.state.lock().unwrap().closed = true;
                Ok(())
            })
        }
    }

    async fn make_test_env() -> (AppConfig, AudioCoordinator, Arc<AgentRuntime>, NativeWakeMonitor) {
        let mut cfg = AppConfig::default();
        cfg.airplay.enabled = false;
        cfg.dual.exit_earcon = None;
        cfg.realtime.playback_tail_ms = 120;
        cfg.realtime.idle_timeout_s = 10;
        cfg.dual.capture.exit_playback_threshold = 0.45;

        let handle = ConfigHandle::new(Arc::new(cfg.clone()));
        let shell = Arc::new(RecordingShell::always_ok());
        let device = Device::with_runner(cfg.device.clone(), shell);
        let playback = Arc::new(PlaybackGate::default());
        let tts = TtsRouter::new(device.clone(), playback.clone());
        let music = Arc::new(MusicService::new(handle.clone(), device.clone()).unwrap());
        let airplay = AirPlayService::start(cfg.airplay.clone()).await.unwrap();

        let mock_player = Arc::new(AplayStreamPlayer::new(StreamAudioSpec {
            command_override: Some(("cat".to_string(), vec![])),
            ..StreamAudioSpec::default()
        }));
        let audio = AudioCoordinator::new(
            device.clone(),
            cfg.device.clone(),
            playback,
            tts,
            music.clone(),
            airplay,
            RuntimeTelemetry::default(),
            Arc::new(TtsHealth::default()),
        )
        .with_stream_player(mock_player);

        let agent = Arc::new(AgentRuntime::new(handle, device, music).await.unwrap());
        let native_monitor = NativeWakeMonitor::disabled();
        (cfg, audio, agent, native_monitor)
    }

    fn make_s16_audio(samples: usize, val: i16) -> Bytes {
        let mut buf = Vec::with_capacity(samples * 2);
        for i in 0..samples {
            let v = if i % 2 == 0 { val } else { -val };
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Bytes::from(buf)
    }

    async fn wait_for_phase(telemetry: &RuntimeTelemetry, target: AgentPhase) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while telemetry.phase() != target {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for phase {target:?} (current {:?})",
                telemetry.phase()
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_until_sent_count(state: &Arc<Mutex<MockDriverState>>, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while state.lock().unwrap().sent_audio.len() < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for sent_audio len >= {count}"
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn half_duplex_gates_uplink_during_speaking_and_resumes_after_tail() {
        let (cfg, audio, agent, native_monitor) = make_test_env().await;
        let telemetry = audio.telemetry.clone();
        let mut native_rx = native_monitor.subscribe();
        let (_wake_tx, wake_rx) = mpsc::channel(8);
        let (audio_tx, audio_rx) = mpsc::channel(16);
        let (mut kws, _stop_tx) = ExternalKwsHandle::from_test_channels(wake_rx, audio_rx);

        let (ev_tx, ev_rx) = mpsc::channel(16);
        let (driver, driver_state) = MockRealtimeDriver::new(ev_rx);

        let run_task = tokio::spawn(async move {
            RealtimeSessionRunner::run_with_driver(
                driver,
                &cfg,
                &audio,
                &agent,
                &mut kws,
                &native_monitor,
                &mut native_rx,
                None,
            )
            .await
        });

        ev_tx
            .send(Ok(RealtimeEvent::SessionReady {
                session_id: "s-hd".into(),
            }))
            .await
            .unwrap();

        // 1) Listening + sys_active=false -> forwarded
        audio_tx
            .send(AudioChunk {
                pcm: Bytes::from_static(&[1, 2, 3, 4]),
                sys_active: false,
            })
            .await
            .unwrap();
        // 2) Listening + sys_active=true -> suppressed
        audio_tx
            .send(AudioChunk {
                pcm: Bytes::from_static(&[5, 6, 7, 8]),
                sys_active: true,
            })
            .await
            .unwrap();
        wait_until_sent_count(&driver_state, 1).await;
        assert_eq!(
            driver_state.lock().unwrap().sent_audio,
            vec![Bytes::from_static(&[1, 2, 3, 4])]
        );

        // 3) Enter Speaking via AssistantAudioDelta -> uplink MUST be gated off
        ev_tx
            .send(Ok(RealtimeEvent::AssistantAudioDelta(make_s16_audio(128, 8000))))
            .await
            .unwrap();
        wait_for_phase(&telemetry, AgentPhase::Speaking).await;
        audio_tx
            .send(AudioChunk {
                pcm: Bytes::from_static(&[9, 10, 11, 12]),
                sys_active: false,
            })
            .await
            .unwrap();
        sleep(Duration::from_millis(40)).await;
        assert_eq!(
            driver_state.lock().unwrap().sent_audio,
            vec![Bytes::from_static(&[1, 2, 3, 4])],
            "uplink PCM must not be transmitted while Speaking"
        );

        // 4) AssistantAudioDone -> wait until tail expires & Listening resumes
        ev_tx.send(Ok(RealtimeEvent::AssistantAudioDone)).await.unwrap();
        ev_tx
            .send(Ok(RealtimeEvent::ResponseDone {
                usage: RealtimeUsage::default(),
            }))
            .await
            .unwrap();
        wait_for_phase(&telemetry, AgentPhase::Listening).await;

        audio_tx
            .send(AudioChunk {
                pcm: Bytes::from_static(&[13, 14, 15, 16]),
                sys_active: false,
            })
            .await
            .unwrap();
        wait_until_sent_count(&driver_state, 2).await;
        assert_eq!(
            driver_state.lock().unwrap().sent_audio,
            vec![
                Bytes::from_static(&[1, 2, 3, 4]),
                Bytes::from_static(&[13, 14, 15, 16]),
            ]
        );

        ev_tx.send(Ok(RealtimeEvent::Closed)).await.unwrap();
        let outcome = timeout(Duration::from_secs(2), run_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(outcome, RealtimeRunOutcome::Completed);
    }

    #[tokio::test]
    async fn wake_barge_in_cancels_response_interrupts_player_and_records_partial_turn() {
        let (cfg, audio, agent, native_monitor) = make_test_env().await;
        let mut native_rx = native_monitor.subscribe();
        let (wake_tx, wake_rx) = mpsc::channel(8);
        let (_audio_tx, audio_rx) = mpsc::channel(16);
        let (mut kws, _stop_tx) = ExternalKwsHandle::from_test_channels(wake_rx, audio_rx);

        let (ev_tx, ev_rx) = mpsc::channel(16);
        let (driver, driver_state) = MockRealtimeDriver::new(ev_rx);
        let agent_ref = agent.clone();

        let run_task = tokio::spawn(async move {
            RealtimeSessionRunner::run_with_driver(
                driver,
                &cfg,
                &audio,
                &agent_ref,
                &mut kws,
                &native_monitor,
                &mut native_rx,
                None,
            )
            .await
        });

        ev_tx
            .send(Ok(RealtimeEvent::SessionReady {
                session_id: "s-wake".into(),
            }))
            .await
            .unwrap();
        ev_tx
            .send(Ok(RealtimeEvent::UserTranscriptCompleted("讲个故事".into())))
            .await
            .unwrap();
        ev_tx
            .send(Ok(RealtimeEvent::AssistantTextDelta("从前有座山".into())))
            .await
            .unwrap();
        ev_tx
            .send(Ok(RealtimeEvent::AssistantAudioDelta(make_s16_audio(128, 9000))))
            .await
            .unwrap();
        sleep(Duration::from_millis(40)).await;

        // Wake word interrupts playback
        wake_tx
            .send(BargeInEvent {
                signal: BargeInSignal::Wake,
                keyword: "豆包豆包".into(),
                score: Some(0.25),
            })
            .await
            .unwrap();
        sleep(Duration::from_millis(50)).await;

        assert_eq!(driver_state.lock().unwrap().canceled_count, 1);
        let (entries, total, _) = agent.conversation_snapshot(10).await;
        assert_eq!(total, 1);
        assert_eq!(entries[0].user, "讲个故事");
        assert_eq!(entries[0].assistant, "从前有座山");
        assert!(entries[0].interrupted);

        ev_tx.send(Ok(RealtimeEvent::Closed)).await.unwrap();
        let outcome = timeout(Duration::from_secs(2), run_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(outcome, RealtimeRunOutcome::Completed);
    }

    #[tokio::test]
    async fn exit_echo_guard_and_threshold_block_false_exit_during_playback() {
        let (cfg, audio, agent, native_monitor) = make_test_env().await;
        let telemetry = audio.telemetry.clone();
        let mut native_rx = native_monitor.subscribe();
        let (wake_tx, wake_rx) = mpsc::channel(8);
        let (_audio_tx, audio_rx) = mpsc::channel(16);
        let (mut kws, _stop_tx) = ExternalKwsHandle::from_test_channels(wake_rx, audio_rx);

        let (ev_tx, ev_rx) = mpsc::channel(16);
        let (driver, _driver_state) = MockRealtimeDriver::new(ev_rx);

        let run_task = tokio::spawn(async move {
            RealtimeSessionRunner::run_with_driver(
                driver,
                &cfg,
                &audio,
                &agent,
                &mut kws,
                &native_monitor,
                &mut native_rx,
                None,
            )
            .await
        });

        ev_tx
            .send(Ok(RealtimeEvent::AssistantTextDelta("可以说关闭按钮".into())))
            .await
            .unwrap();
        ev_tx
            .send(Ok(RealtimeEvent::AssistantAudioDelta(make_s16_audio(128, 7000))))
            .await
            .unwrap();
        wait_for_phase(&telemetry, AgentPhase::Speaking).await;

        // 1) Low score (< 0.45) during playback must be ignored
        wake_tx
            .send(BargeInEvent {
                signal: BargeInSignal::ExitCmd,
                keyword: "__CMD_EXIT__".into(),
                score: Some(0.32),
            })
            .await
            .unwrap();
        sleep(Duration::from_millis(30)).await;
        assert!(!run_task.is_finished(), "low-score exit should be blocked");

        // 2) High score (>= 0.45) matching AI spoken exit word via ExitEchoGuard must ALSO be blocked
        wake_tx
            .send(BargeInEvent {
                signal: BargeInSignal::ExitCmd,
                keyword: "__CMD_EXIT__".into(),
                score: Some(0.85),
            })
            .await
            .unwrap();
        sleep(Duration::from_millis(30)).await;
        assert!(
            !run_task.is_finished(),
            "ExitEchoGuard must block __CMD_EXIT__ while AI says '关闭'"
        );

        // 3) Finish audio + tail -> ExitEchoGuard resets -> real ExitCmd exits cleanly
        ev_tx.send(Ok(RealtimeEvent::AssistantAudioDone)).await.unwrap();
        ev_tx
            .send(Ok(RealtimeEvent::ResponseDone {
                usage: RealtimeUsage::default(),
            }))
            .await
            .unwrap();
        wait_for_phase(&telemetry, AgentPhase::Listening).await;

        wake_tx
            .send(BargeInEvent {
                signal: BargeInSignal::ExitCmd,
                keyword: "__CMD_EXIT__".into(),
                score: Some(0.60),
            })
            .await
            .unwrap();
        let outcome = timeout(Duration::from_secs(2), run_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(outcome, RealtimeRunOutcome::Completed);
    }

    #[tokio::test]
    async fn native_xiaoai_transcript_returns_native_yield_and_connect_error_falls_back() {
        // 1) Native XiaoAI utterance triggers NativeYield
        let (cfg, audio, agent, native_monitor) = make_test_env().await;
        let mut native_rx = native_monitor.subscribe();
        let (_wake_tx, wake_rx) = mpsc::channel(8);
        let (_audio_tx, audio_rx) = mpsc::channel(16);
        let (mut kws, _stop_tx) = ExternalKwsHandle::from_test_channels(wake_rx, audio_rx);
        let (ev_tx, ev_rx) = mpsc::channel(8);
        let (driver, _) = MockRealtimeDriver::new(ev_rx);

        ev_tx
            .send(Ok(RealtimeEvent::UserTranscriptCompleted(
                "小爱同学，打开客厅灯".into(),
            )))
            .await
            .unwrap();
        let outcome = RealtimeSessionRunner::run_with_driver(
            driver,
            &cfg,
            &audio,
            &agent,
            &mut kws,
            &native_monitor,
            &mut native_rx,
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome, RealtimeRunOutcome::NativeYield);

        // 2) Initial connect failure returns FallbackToCascade
        let (cfg2, audio2, agent2, native_monitor2) = make_test_env().await;
        let mut native_rx2 = native_monitor2.subscribe();
        let (_wake_tx2, wake_rx2) = mpsc::channel(8);
        let (_audio_tx2, audio_rx2) = mpsc::channel(16);
        let (mut kws2, _stop_tx2) = ExternalKwsHandle::from_test_channels(wake_rx2, audio_rx2);

        let fallback_outcome = RealtimeSessionRunner::run_with_driver_result::<MockRealtimeDriver>(
            Err(anyhow::anyhow!("ws handshake 401")),
            &cfg2,
            &audio2,
            &agent2,
            &mut kws2,
            &native_monitor2,
            &mut native_rx2,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            fallback_outcome,
            RealtimeRunOutcome::FallbackToCascade { initial_query: None }
        );
    }

    #[tokio::test]
    async fn transcript_exit_guard_ignores_negation_and_accepts_farewell() {
        let (cfg, audio, agent, native_monitor) = make_test_env().await;
        let mut native_rx = native_monitor.subscribe();
        let (_wake_tx, wake_rx) = mpsc::channel(8);
        let (_audio_tx, audio_rx) = mpsc::channel(16);
        let (mut kws, _stop_tx) = ExternalKwsHandle::from_test_channels(wake_rx, audio_rx);
        let (ev_tx, ev_rx) = mpsc::channel(8);
        let (driver, _) = MockRealtimeDriver::new(ev_rx);

        let run_task = tokio::spawn(async move {
            RealtimeSessionRunner::run_with_driver(
                driver,
                &cfg,
                &audio,
                &agent,
                &mut kws,
                &native_monitor,
                &mut native_rx,
                None,
            )
            .await
        });

        // Negation must NOT exit
        ev_tx
            .send(Ok(RealtimeEvent::UserTranscriptCompleted("不想关闭".into())))
            .await
            .unwrap();
        sleep(Duration::from_millis(40)).await;
        assert!(!run_task.is_finished(), "'不想关闭' must not exit session");

        // Polite farewell MUST exit cleanly
        ev_tx
            .send(Ok(RealtimeEvent::UserTranscriptCompleted("豆包再见啦".into())))
            .await
            .unwrap();
        let outcome = timeout(Duration::from_secs(2), run_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(outcome, RealtimeRunOutcome::Completed);
    }

    #[tokio::test]
    async fn smart_turn_extends_wait_for_unfinished_and_commits_complete_promptly() {
        let (mut cfg, audio, agent, native_monitor) = make_test_env().await;
        cfg.realtime.smart_turn.enabled = true;
        cfg.realtime.smart_turn.complete_quiet_ms = 60;
        cfg.realtime.smart_turn.unfinished_quiet_ms = 220;

        let mut native_rx = native_monitor.subscribe();
        let (_wake_tx, wake_rx) = mpsc::channel(8);
        let (_audio_tx, audio_rx) = mpsc::channel(16);
        let (mut kws, _stop_tx) = ExternalKwsHandle::from_test_channels(wake_rx, audio_rx);
        let (ev_tx, ev_rx) = mpsc::channel(16);
        let (driver, driver_state) = MockRealtimeDriver::new(ev_rx);

        let run_task = tokio::spawn(async move {
            RealtimeSessionRunner::run_with_driver(
                driver,
                &cfg,
                &audio,
                &agent,
                &mut kws,
                &native_monitor,
                &mut native_rx,
                None,
            )
            .await
        });

        ev_tx
            .send(Ok(RealtimeEvent::SessionReady {
                session_id: "s-smart".into(),
            }))
            .await
            .unwrap();

        // 1) Unfinished Chinese opener must stay open past 60 ms
        ev_tx
            .send(Ok(RealtimeEvent::UserTranscriptDelta("因为".into())))
            .await
            .unwrap();
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            driver_state.lock().unwrap().committed_count,
            0,
            "unfinished phrase must NOT commit at complete_quiet_ms"
        );

        // 2) Complete phrase commits shortly after 60 ms
        ev_tx
            .send(Ok(RealtimeEvent::UserTranscriptDelta(
                "因为明天要下雨所以带伞".into(),
            )))
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while driver_state.lock().unwrap().committed_count < 1 {
            assert!(Instant::now() < deadline, "smart turn failed to commit");
            sleep(Duration::from_millis(15)).await;
        }

        ev_tx.send(Ok(RealtimeEvent::Closed)).await.unwrap();
        let outcome = timeout(Duration::from_secs(2), run_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(outcome, RealtimeRunOutcome::Completed);
    }

    #[tokio::test]
    async fn daily_usage_accumulates_response_tokens_and_blocks_exceeded_session() {
        let dir = std::env::temp_dir().join(format!(
            "xiaoai_runner_usage_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let (mut cfg, audio, agent, native_monitor) = make_test_env().await;
        cfg.realtime.daily_usage.state_dir = dir.clone();
        cfg.realtime.daily_usage.daily_token_limit = 500;

        let mut native_rx = native_monitor.subscribe();
        let (_wake_tx, wake_rx) = mpsc::channel(8);
        let (_audio_tx, audio_rx) = mpsc::channel(16);
        let (mut kws, _stop_tx) = ExternalKwsHandle::from_test_channels(wake_rx, audio_rx);
        let (ev_tx, ev_rx) = mpsc::channel(8);
        let (driver, _) = MockRealtimeDriver::new(ev_rx);

        ev_tx
            .send(Ok(RealtimeEvent::ResponseDone {
                usage: RealtimeUsage {
                    input_text_tokens: 200,
                    output_audio_tokens: 350,
                    ..RealtimeUsage::default()
                },
            }))
            .await
            .unwrap();
        ev_tx.send(Ok(RealtimeEvent::Closed)).await.unwrap();

        let outcome = RealtimeSessionRunner::run_with_driver(
            driver,
            &cfg,
            &audio,
            &agent,
            &mut kws,
            &native_monitor,
            &mut native_rx,
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome, RealtimeRunOutcome::Completed);

        let guard = DailyUsageGuard::from_config(&cfg.realtime.daily_usage);
        assert_eq!(guard.read_today(), 550);
        assert!(guard.is_limit_exceeded());

        // Subsequent session must be blocked pre-connect and fall back to Cascade.
        let (_wake_tx2, wake_rx2) = mpsc::channel(8);
        let (_audio_tx2, audio_rx2) = mpsc::channel(16);
        let (mut kws2, _stop_tx2) = ExternalKwsHandle::from_test_channels(wake_rx2, audio_rx2);
        let mut native_rx2 = native_monitor.subscribe();
        let (_ev_tx2, ev_rx2) = mpsc::channel(4);
        let (driver2, state2) = MockRealtimeDriver::new(ev_rx2);

        let blocked_outcome = RealtimeSessionRunner::run_with_driver_result(
            Ok(driver2),
            &cfg,
            &audio,
            &agent,
            &mut kws2,
            &native_monitor,
            &mut native_rx2,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            blocked_outcome,
            RealtimeRunOutcome::FallbackToCascade { initial_query: None }
        );
        assert!(state2.lock().unwrap().closed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stop_key_interrupts_speaking_and_returns_to_listening() {
        let (cfg, audio, agent, native_monitor) = make_test_env().await;
        let telemetry = audio.telemetry.clone();
        let mut native_rx = native_monitor.subscribe();
        let (_wake_tx, wake_rx) = mpsc::channel(8);
        let (_audio_tx, audio_rx) = mpsc::channel(16);
        let (mut kws, _stop_tx) = ExternalKwsHandle::from_test_channels(wake_rx, audio_rx);
        let (stop_key_tx, mut stop_key_rx) = mpsc::channel(4);
        let (ev_tx, ev_rx) = mpsc::channel(8);
        let (driver, driver_state) = MockRealtimeDriver::new(ev_rx);

        let run_task = tokio::spawn(async move {
            RealtimeSessionRunner::run_with_driver(
                driver,
                &cfg,
                &audio,
                &agent,
                &mut kws,
                &native_monitor,
                &mut native_rx,
                Some(&mut stop_key_rx),
            )
            .await
        });

        ev_tx
            .send(Ok(RealtimeEvent::SessionReady {
                session_id: "s-key".into(),
            }))
            .await
            .unwrap();
        ev_tx
            .send(Ok(RealtimeEvent::AssistantAudioDelta(make_s16_audio(128, 7000))))
            .await
            .unwrap();
        wait_for_phase(&telemetry, AgentPhase::Speaking).await;

        stop_key_tx.send(()).await.unwrap();
        wait_for_phase(&telemetry, AgentPhase::Listening).await;
        assert_eq!(driver_state.lock().unwrap().canceled_count, 1);

        ev_tx.send(Ok(RealtimeEvent::Closed)).await.unwrap();
        let outcome = timeout(Duration::from_secs(2), run_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(outcome, RealtimeRunOutcome::Completed);
    }
}

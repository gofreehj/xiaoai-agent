//! Conversation session orchestration, local intent dispatch, and control-plane commands.

pub mod intent;
pub mod pipeline;

use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::agent::AgentRuntime;
use crate::asr::AsrClient;
use crate::audio::{AudioCoordinator, CaptureOutcome, FinishMode, VolumeAction};
use crate::capture::{
    record_utterance_outcome, record_utterance_server_vad_streaming_outcome,
    record_utterance_streaming_outcome,
};
use crate::config::{AppConfig, ConfigHandle, TtsProvider};
use crate::frontend::mipns::{drain_stale_followup_turns, request_followup_capture};
use crate::runtime::telemetry::AgentPhase;
use crate::session::intent::{route_local_intent, LocalIntent};
use crate::session::pipeline::{InFlightTurn, TurnPipeline, TurnPipelineRequest};
use crate::speech::{ActivationMode, SpeechHandle, SpeechTurn};
use crate::tts::{ExitAction, MusicVoiceControl};

pub const ASR_SERVICE_ERROR_PROMPT: &str = "抱歉，语音识别服务遇到问题，请稍后重试";
pub const WAKE_ACK_PROMPT: &str = "我在，请讲";
pub const EXIT_FAREWELL_PROMPT: &str = "好的，再见";

pub async fn play_optional_earcon(audio: &AudioCoordinator, path: Option<&Path>) -> bool {
    let Some(path) = path else {
        return false;
    };
    audio.play_earcon(&path.to_string_lossy()).await
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ControlCommand {
    Stop,
    Speak(String),
    Chat(String),
    Reload,
    ClearHistory,
    Volume(VolumeAction),
}

/// 中断原因。决定 session 重置的日志理由，以及收尾失败该按 debug 还是 warn 记。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptCause {
    /// 新的唤醒词打断了上一轮。
    Wake,
    /// 会话空闲超时（采集等不到人说话、等不到 follow-up 流、前端空闲结束流）。
    IdleTimeout,
    /// 语音前端正常关闭了流（follow-up 发送端没了），不是故障。
    FrontendStopped,
    /// 采集中途失败（mipns 取消流、ASR 出错、LLM/TTS 抛出错误等）。
    Failure(&'static str),
}

impl InterruptCause {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Wake => "wake keyword",
            Self::IdleTimeout => "session idle timeout",
            Self::FrontendStopped => "speech frontend stopped",
            Self::Failure(reason) => reason,
        }
    }

    /// 这条收尾是否属于预期内的正常结束。`false` 只有 `Failure`。
    pub fn expected(self) -> bool {
        !matches!(self, Self::Failure(_))
    }
}

/// Context passed into [`handle_local_intent`] to unify `exclusive` and `dual` local-intent handling.
pub struct LocalIntentContext<'a> {
    pub is_followup: bool,
    pub answered_turns: u32,
    pub consecutive_empty_or_noise: &'a mut u32,
    pub max_empty_retries: u32,
    pub allow_wake_ack: bool,
    pub enter_prompt: &'a str,
    pub exit_prompt: &'a str,
    pub prefer_exit_earcon: bool,
    pub speak_exit_on_noise_exhausted: bool,
}

/// Outcome of routing and executing a local voice intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalIntentOutcome {
    /// Continue listening for the next utterance (e.g. wake-only ack or retryable empty/noise).
    ContinueListening,
    /// Volume was adjusted locally without invoking the LLM.
    VolumeHandled,
    /// Session should finish cleanly (music control, exit command, or exhausted noise retries).
    EndSession,
    /// Forward cleaned user text to the streaming LLM + TTS pipeline.
    Chat(String),
}

/// Speak a short local prompt via `AudioCoordinator` with `FinishMode::WaitForEnd`.
pub async fn speak_prompt_wait(audio: &AudioCoordinator, config: &AppConfig, text: &str) {
    if let Err(err) = audio
        .speak_once(&config.tts, text, FinishMode::WaitForEnd)
        .await
    {
        audio.tts_health.on_failure(&err);
    }
}

/// Route `raw_text` through [`route_local_intent`] and execute local actions immediately
/// without invoking the LLM.
pub async fn handle_local_intent(
    raw_text: &str,
    config: &AppConfig,
    audio: &AudioCoordinator,
    agent: &Arc<AgentRuntime>,
    ctx: &mut LocalIntentContext<'_>,
) -> LocalIntentOutcome {
    let has_music = audio.music.has_current().await;
    match route_local_intent(raw_text, has_music, &config.dual.exit_keywords) {
        LocalIntent::WakeWordOnly => {
            *ctx.consecutive_empty_or_noise = 0;
            info!(
                text = raw_text.trim(),
                "检测到纯唤醒词/在场确认，回应并继续收音"
            );
            if ctx.allow_wake_ack {
                audio.set_phase(AgentPhase::Speaking).await;
                if !play_optional_earcon(audio, config.dual.enter_earcon.as_deref()).await {
                    speak_prompt_wait(audio, config, ctx.enter_prompt).await;
                }
            }
            LocalIntentOutcome::ContinueListening
        }
        LocalIntent::EmptyOrNoise => {
            *ctx.consecutive_empty_or_noise += 1;
            let can_retry = (ctx.is_followup || ctx.answered_turns == 0)
                && *ctx.consecutive_empty_or_noise < ctx.max_empty_retries;
            if can_retry {
                info!(
                    text = raw_text.trim(),
                    attempt = *ctx.consecutive_empty_or_noise,
                    "filtered empty/filler ASR result; continuing capture without premature hangup"
                );
                return LocalIntentOutcome::ContinueListening;
            }
            info!(
                text = raw_text.trim(),
                attempts = *ctx.consecutive_empty_or_noise,
                "empty/filler ASR result after retries; ending session"
            );
            if ctx.speak_exit_on_noise_exhausted {
                audio.set_phase(AgentPhase::Speaking).await;
                if !(ctx.prefer_exit_earcon
                    && play_optional_earcon(audio, config.dual.exit_earcon.as_deref()).await)
                {
                    speak_prompt_wait(audio, config, ctx.exit_prompt).await;
                }
            }
            agent.reset_session("empty ASR result").await;
            LocalIntentOutcome::EndSession
        }
        LocalIntent::Volume(action) => {
            *ctx.consecutive_empty_or_noise = 0;
            info!(?action, "本地快捷控制：调节音量");
            if let Err(err) = audio.execute_volume(action).await {
                warn!(?action, error = %err, "本地音量控制失败");
            }
            LocalIntentOutcome::VolumeHandled
        }
        LocalIntent::Music(control) => {
            *ctx.consecutive_empty_or_noise = 0;
            match control {
                MusicVoiceControl::Stop => {
                    info!(text = raw_text.trim(), "本地快捷控制：停止音乐播放");
                    let _ = audio.music.stop().await;
                    audio.set_phase(AgentPhase::Speaking).await;
                    speak_prompt_wait(audio, config, "好的，已停止播放").await;
                }
                MusicVoiceControl::Pause => {
                    info!(text = raw_text.trim(), "本地快捷控制：暂停音乐播放");
                    let _ = audio.music.pause().await;
                    audio.set_phase(AgentPhase::Speaking).await;
                    speak_prompt_wait(audio, config, "好的，已暂停播放").await;
                }
                MusicVoiceControl::Resume => {
                    info!(text = raw_text.trim(), "本地快捷控制：继续音乐播放");
                    let _ = audio.music.resume().await;
                }
                MusicVoiceControl::Next => {
                    info!(text = raw_text.trim(), "本地快捷控制：下一首音乐");
                    let _ = audio.music.next().await;
                }
                MusicVoiceControl::Previous => {
                    info!(text = raw_text.trim(), "本地快捷控制：上一首音乐");
                    let _ = audio.music.previous().await;
                }
            }
            LocalIntentOutcome::EndSession
        }
        LocalIntent::Exit(ExitAction::Silent) => {
            *ctx.consecutive_empty_or_noise = 0;
            info!(text = raw_text.trim(), "检测到静默退出对话指令");
            agent.reset_session("user exit command").await;
            LocalIntentOutcome::EndSession
        }
        LocalIntent::Exit(ExitAction::Farewell) => {
            *ctx.consecutive_empty_or_noise = 0;
            info!(text = raw_text.trim(), "检测到礼貌道别退出对话指令");
            audio.set_phase(AgentPhase::Speaking).await;
            if !(ctx.prefer_exit_earcon
                && play_optional_earcon(audio, config.dual.exit_earcon.as_deref()).await)
            {
                speak_prompt_wait(audio, config, ctx.exit_prompt).await;
            }
            agent.reset_session("user exit command").await;
            LocalIntentOutcome::EndSession
        }
        LocalIntent::Chat(cleaned) => {
            *ctx.consecutive_empty_or_noise = 0;
            LocalIntentOutcome::Chat(cleaned)
        }
    }
}

/// Shared session orchestrator driving multi-turn capture, local intents, and streaming LLM+TTS.
#[derive(Clone)]
pub struct SessionOrchestrator {
    pub config: ConfigHandle,
    pub asr: AsrClient,
    pub agent: Arc<AgentRuntime>,
    pub audio: AudioCoordinator,
    pub failures: Arc<AtomicU32>,
    pub in_flight: InFlightTurn,
}

impl SessionOrchestrator {
    pub fn new(
        config: ConfigHandle,
        asr: AsrClient,
        agent: Arc<AgentRuntime>,
        audio: AudioCoordinator,
        failures: Arc<AtomicU32>,
    ) -> Self {
        Self {
            config,
            asr,
            agent,
            audio,
            failures,
            in_flight: InFlightTurn::default(),
        }
    }

    pub fn pipeline(&self) -> TurnPipeline<'_> {
        TurnPipeline::new(&self.agent, &self.audio, &self.in_flight)
    }

    /// Best-effort protocol teardown for `mipns` turns on wake barge-in, idle timeout, or failure.
    ///
    /// Never issues `stop_capture` (owned by `capture.rs`).
    pub async fn interrupt_mipns_turn(
        &self,
        speech: &SpeechHandle,
        turn_id: u64,
        cause: InterruptCause,
    ) {
        if let Err(err) = speech.finish_dialog(turn_id).await {
            if cause.expected() {
                debug!(
                    turn_id,
                    cause = cause.reason(),
                    error = %err,
                    "收尾的 finish_dialog 没有送达（属于预期路径）"
                );
            } else {
                warn!(
                    turn_id,
                    cause = cause.reason(),
                    error = %err,
                    "中断收尾时 finish_dialog 失败"
                );
            }
        }
        self.agent.reset_session(cause.reason()).await;
    }

    /// Execute a full `mipns` exclusive-mode multi-turn session with cleanup and failure accounting.
    pub async fn run_mipns_turn(
        &self,
        speech: SpeechHandle,
        first_turn: SpeechTurn,
        followup_rx: mpsc::Receiver<SpeechTurn>,
        active_turn_id: Arc<AtomicU64>,
    ) -> anyhow::Result<()> {
        let result = self
            .run_mipns_session(
                speech.clone(),
                first_turn,
                followup_rx,
                active_turn_id.clone(),
            )
            .await;
        if result.is_err() {
            self.interrupt_mipns_turn(
                &speech,
                active_turn_id.load(Ordering::Relaxed),
                InterruptCause::Failure("turn failed"),
            )
            .await;
            self.failures.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failures.store(0, Ordering::Relaxed);
        }
        self.audio.release_dialog_focus().await;
        result
    }

    async fn run_mipns_session(
        &self,
        speech: SpeechHandle,
        mut turn: SpeechTurn,
        mut followup_rx: mpsc::Receiver<SpeechTurn>,
        active_turn_id: Arc<AtomicU64>,
    ) -> anyhow::Result<()> {
        let mut consecutive_empty_or_noise: u32 = 0;
        let mut answered_turns_in_session: u32 = 0;

        loop {
            let config = self.config.get().await;
            let idle_timeout =
                Duration::from_secs_f64(config.runtime.session_idle_timeout_s.max(1.0));
            let turn_id = turn.id();
            active_turn_id.store(turn_id, Ordering::Relaxed);
            self.audio.set_phase(AgentPhase::Listening).await;

            let maybe_stream = match self
                .asr
                .start_streaming_transcription(config.capture.sample_rate)
                .await
            {
                Ok(stream) => stream,
                Err(err) => {
                    self.speak_service_error(ASR_SERVICE_ERROR_PROMPT).await;
                    return Err(err.context("ASR failed after retries"));
                }
            };

            let (text, asr_started_at, asr_ms, pcm_bytes) = if let Some(mut stream) = maybe_stream {
                if stream.server_vad_enabled() {
                    let audio_start = self.audio.clone();
                    let audio_stop = self.audio.clone();
                    let speech_stopped_at = Arc::new(std::sync::Mutex::new(None::<Instant>));
                    let stop_stamp = speech_stopped_at.clone();
                    let text_opt = match record_utterance_server_vad_streaming_outcome(
                        config.capture.clone(),
                        idle_timeout,
                        &self.audio.playback,
                        &mut turn,
                        &mut stream,
                        move || {
                            let audio = audio_start.clone();
                            async move {
                                audio.set_phase(AgentPhase::UserSpeaking).await;
                            }
                        },
                        move || {
                            let audio = audio_stop.clone();
                            let stop_stamp = stop_stamp.clone();
                            async move {
                                if let Ok(mut guard) = stop_stamp.lock() {
                                    *guard = Some(Instant::now());
                                }
                                audio.set_phase(AgentPhase::Thinking).await;
                            }
                        },
                    )
                    .await
                    {
                        Ok(opt) => opt,
                        Err(err) if is_capture_timeout(&err) => {
                            stream.close().await;
                            info!(turn_id, "session idle timeout");
                            self.interrupt_mipns_turn(&speech, turn_id, InterruptCause::IdleTimeout)
                                .await;
                            return Ok(());
                        }
                        Err(err) => {
                            stream.close().await;
                            self.speak_service_error(ASR_SERVICE_ERROR_PROMPT).await;
                            return Err(err.context("ASR failed after retries"));
                        }
                    };
                    let Some(text) = text_opt else {
                        stream.close().await;
                        info!(turn_id, "session idle end (server VAD stream ended cleanly)");
                        self.interrupt_mipns_turn(&speech, turn_id, InterruptCause::IdleTimeout)
                            .await;
                        return Ok(());
                    };
                    let stopped = speech_stopped_at
                        .lock()
                        .ok()
                        .and_then(|g| *g)
                        .unwrap_or_else(Instant::now);
                    (text, stopped, stopped.elapsed().as_millis() as u64, None)
                } else {
                    let appender = stream.appender();
                    let appender_for_chunk = appender.clone();
                    let appender_for_reject = appender.clone();
                    let audio_start = self.audio.clone();
                    let outcome = match record_utterance_streaming_outcome(
                        config.capture.clone(),
                        idle_timeout,
                        &self.audio.playback,
                        &mut turn,
                        move || {
                            let audio = audio_start.clone();
                            async move {
                                audio.set_phase(AgentPhase::UserSpeaking).await;
                            }
                        },
                        move |bytes| {
                            let appender = appender_for_chunk.clone();
                            async move { appender.append_pcm(bytes).await }
                        },
                        move || {
                            let appender = appender_for_reject.clone();
                            async move { appender.clear().await }
                        },
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) if is_capture_timeout(&err) => {
                            stream.close().await;
                            info!(turn_id, "session idle timeout");
                            self.interrupt_mipns_turn(&speech, turn_id, InterruptCause::IdleTimeout)
                                .await;
                            return Ok(());
                        }
                        Err(err) => {
                            stream.close().await;
                            return Err(err);
                        }
                    };
                    if matches!(outcome, CaptureOutcome::IdleEnd) {
                        stream.close().await;
                        info!(turn_id, "session idle end (mipns stream ended without speech)");
                        self.interrupt_mipns_turn(&speech, turn_id, InterruptCause::IdleTimeout)
                            .await;
                        return Ok(());
                    }

                    self.audio.set_phase(AgentPhase::Thinking).await;
                    let asr_started_at = Instant::now();
                    let text = match stream.commit_and_transcribe().await {
                        Ok(text) => text,
                        Err(err) => {
                            self.speak_service_error(ASR_SERVICE_ERROR_PROMPT).await;
                            return Err(err.context("ASR failed after retries"));
                        }
                    };
                    (text, asr_started_at, asr_started_at.elapsed().as_millis() as u64, None)
                }
            } else {
                let audio_start = self.audio.clone();
                let outcome = match record_utterance_outcome(
                    config.capture.clone(),
                    idle_timeout,
                    &self.audio.playback,
                    &mut turn,
                    move || {
                        let audio = audio_start.clone();
                        async move {
                            audio.set_phase(AgentPhase::UserSpeaking).await;
                        }
                    },
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(err) if is_capture_timeout(&err) => {
                        info!(turn_id, "session idle timeout");
                        self.interrupt_mipns_turn(&speech, turn_id, InterruptCause::IdleTimeout)
                            .await;
                        return Ok(());
                    }
                    Err(err) => return Err(err),
                };
                let pcm = match outcome {
                    CaptureOutcome::Utterance(pcm) => pcm,
                    CaptureOutcome::IdleEnd => {
                        info!(turn_id, "session idle end (mipns stream ended without speech)");
                        self.interrupt_mipns_turn(&speech, turn_id, InterruptCause::IdleTimeout)
                            .await;
                        return Ok(());
                    }
                };
                let captured_bytes = pcm.len();

                self.audio.set_phase(AgentPhase::Thinking).await;
                let asr_started_at = Instant::now();
                let text = match self
                    .asr
                    .transcribe_pcm(&pcm, config.capture.sample_rate)
                    .await
                {
                    Ok(text) => text,
                    Err(err) => {
                        self.speak_service_error(ASR_SERVICE_ERROR_PROMPT).await;
                        return Err(err.context("ASR failed after retries"));
                    }
                };
                (
                    text,
                    asr_started_at,
                    asr_started_at.elapsed().as_millis() as u64,
                    Some(captured_bytes),
                )
            };

            let mut intent_ctx = LocalIntentContext {
                is_followup: turn.activation() == ActivationMode::FollowUp,
                answered_turns: answered_turns_in_session,
                consecutive_empty_or_noise: &mut consecutive_empty_or_noise,
                max_empty_retries: 2,
                allow_wake_ack: true,
                enter_prompt: WAKE_ACK_PROMPT,
                exit_prompt: EXIT_FAREWELL_PROMPT,
                prefer_exit_earcon: false,
                speak_exit_on_noise_exhausted: false,
            };

            let command = match handle_local_intent(
                &text,
                &config,
                &self.audio,
                &self.agent,
                &mut intent_ctx,
            )
            .await
            {
                LocalIntentOutcome::ContinueListening => {
                    match self
                        .advance_to_followup_turn(&speech, turn_id, idle_timeout, &mut followup_rx)
                        .await?
                    {
                        Some(next_turn) => {
                            turn = next_turn;
                            continue;
                        }
                        None => return Ok(()),
                    }
                }
                LocalIntentOutcome::EndSession | LocalIntentOutcome::VolumeHandled => {
                    speech.finish_dialog(turn_id).await?;
                    return Ok(());
                }
                LocalIntentOutcome::Chat(cleaned) => cleaned,
            };

            info!(turn_id, asr_ms, "USER_ASR text={command}");

            let playback_token = self.audio.playback.current();
            let reply = self
                .pipeline()
                .execute(TurnPipelineRequest {
                    config: &config,
                    turn_id,
                    source: "exclusive",
                    query: &command,
                    asr_started_at,
                    asr_ms,
                    vad_silence_ms: None,
                    speech_voiced_ms: None,
                    pcm_bytes,
                    finish_mode: FinishMode::DetachForQuietWindow,
                })
                .await?;
            answered_turns_in_session += 1;

            if reply.should_end {
                if config.tts.provider != TtsProvider::Native {
                    self.audio.wait_quiet_window_settled(playback_token).await;
                    self.audio.music.ensure_playing_after_external_tts().await;
                }
                info!("agent ended conversation: {}", reply.end_reason);
                speech.finish_dialog(turn_id).await?;
                self.agent.reset_session("agent ended conversation").await;
                return Ok(());
            }

            // 若本轮点播了音乐（且未处于暂停状态），等待播报结束后再确保播放并结束收音，
            // 防止麦克风把整首歌录进 ASR 自激。
            if self.audio.music.is_actively_playing().await {
                if config.tts.provider != TtsProvider::Native {
                    self.audio.wait_quiet_window_settled(playback_token).await;
                    self.audio.music.ensure_playing_after_external_tts().await;
                }
                info!(
                    turn_id,
                    "音乐正在播放中，自动结束连续收音以防麦克风回采音乐声"
                );
                speech.finish_dialog(turn_id).await?;
                return Ok(());
            }

            if config.tts.provider != TtsProvider::Native {
                self.audio.music.ensure_playing_after_external_tts().await;
            }

            match self
                .advance_to_followup_turn(&speech, turn_id, idle_timeout, &mut followup_rx)
                .await?
            {
                Some(next_turn) => {
                    turn = next_turn;
                }
                None => return Ok(()),
            }
        }
    }

    async fn advance_to_followup_turn(
        &self,
        speech: &SpeechHandle,
        turn_id: u64,
        idle_timeout: Duration,
        followup_rx: &mut mpsc::Receiver<SpeechTurn>,
    ) -> anyhow::Result<Option<SpeechTurn>> {
        drain_stale_followup_turns(followup_rx, speech).await;
        speech.continue_dialog(turn_id).await?;
        request_followup_capture(&self.audio.device).await?;
        self.audio.set_phase(AgentPhase::Listening).await;
        match timeout(idle_timeout, followup_rx.recv()).await {
            Ok(Some(next_turn)) => Ok(Some(next_turn)),
            Ok(None) => {
                self.interrupt_mipns_turn(speech, turn_id, InterruptCause::FrontendStopped)
                    .await;
                Ok(None)
            }
            Err(_) => {
                info!(turn_id, "timed out waiting for follow-up stream");
                self.interrupt_mipns_turn(speech, turn_id, InterruptCause::IdleTimeout)
                    .await;
                Ok(None)
            }
        }
    }

    /// 服务错误提示故意走本地 `device.speak`，避免外部 TTS 同步不可用时完全无声。
    async fn speak_service_error(&self, text: &str) {
        self.audio.set_phase(AgentPhase::Speaking).await;
        if let Err(err) = self.audio.device.speak(text).await {
            warn!("failed to speak service error prompt: {err:?}");
        }
    }
}

pub fn is_capture_timeout(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains("timed out waiting for user speech")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentTurnResult;
    use crate::airplay::AirPlayService;
    use crate::device::Device;
    use crate::music::MusicService;
    use crate::runtime::telemetry::RuntimeTelemetry;
    use crate::session::pipeline::MID_STREAM_INTERRUPTED_PROMPT;
    use crate::shell::RecordingShell;
    use crate::speech::{SpeechInput, TestCommand};
    use crate::tts::{PlaybackGate, TtsHealth, TtsRouter};

    async fn test_orchestrator(shell: Arc<RecordingShell>) -> SessionOrchestrator {
        let mut cfg = AppConfig::default();
        cfg.agent.tools_enabled = false;
        cfg.airplay.enabled = false;
        cfg.runtime.session_idle_timeout_s = 1.0;
        let config = ConfigHandle::new(Arc::new(cfg.clone()));
        let device = Device::with_runner(cfg.device.clone(), shell);
        let asr = AsrClient::new(cfg.asr.clone()).expect("default ASR config should succeed");
        let music = Arc::new(MusicService::new(config.clone(), device.clone()).unwrap());
        let airplay = AirPlayService::start(cfg.airplay.clone()).await.unwrap();
        let agent = Arc::new(
            AgentRuntime::new(config.clone(), device.clone(), music.clone())
                .await
                .unwrap(),
        );
        let playback = Arc::new(PlaybackGate::default());
        let tts = TtsRouter::new(device.clone(), playback.clone());
        let audio = AudioCoordinator::new(
            device,
            cfg.device.clone(),
            playback,
            tts,
            music,
            airplay,
            RuntimeTelemetry::default(),
            Arc::new(TtsHealth::default()),
        );
        SessionOrchestrator::new(config, asr, agent, audio, Arc::new(AtomicU32::new(0)))
    }

    /// (a) `CaptureOutcome::IdleEnd` (mipns sends `SpeechInput::End` without speech) must return
    /// `Ok(())`, finish the dialog cleanly as `IdleTimeout`, and NEVER increment `failures`.
    #[tokio::test]
    async fn capture_outcome_idle_end_returns_ok_without_incrementing_failures() {
        let shell = Arc::new(RecordingShell::always_ok());
        let orch = test_orchestrator(shell).await;
        // Pre-seed failures to 2 to verify IdleEnd resets it to 0 rather than incrementing to 3.
        orch.failures.store(2, Ordering::Relaxed);

        let (turn, input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(31);
        input_tx.send(SpeechInput::End).await.unwrap();
        let (_tx, followup_rx) = mpsc::channel(2);
        let active_turn_id = Arc::new(AtomicU64::new(turn.id()));

        let res = orch
            .run_mipns_turn(handle, turn, followup_rx, active_turn_id)
            .await;

        assert!(res.is_ok(), "IdleEnd must be a normal ending, got: {res:?}");
        assert_eq!(
            orch.failures.load(Ordering::Relaxed),
            0,
            "IdleEnd must never increment failures"
        );
        assert_eq!(
            orch.agent.reset_reason_for_test().await,
            "session idle timeout"
        );
        assert_eq!(
            *recorder.lock().await,
            vec![TestCommand::Finish {
                continue_dialog: false
            }],
            "IdleEnd must finish dialog without re-issuing stop_capture"
        );
    }

    /// (b) `LocalIntent::Volume` executes `mphelper volume_up` immediately without calling LLM.
    #[tokio::test]
    async fn local_intent_volume_executes_mphelper_volume_up_without_llm() {
        let shell = Arc::new(RecordingShell::always_ok());
        let orch = test_orchestrator(shell.clone()).await;
        let config = orch.config.get().await;
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

        let outcome = handle_local_intent(
            "小爱同学，声音大一点",
            &config,
            &orch.audio,
            &orch.agent,
            &mut ctx,
        )
        .await;

        assert_eq!(outcome, LocalIntentOutcome::VolumeHandled);
        assert_eq!(
            shell.count_matching("mphelper volume_up"),
            1,
            "Volume Up must run mphelper volume_up"
        );
        assert_eq!(
            orch.agent.history_len_for_test().await,
            0,
            "Volume command must not invoke LLM or mutate conversation history"
        );
    }

    /// (c) `LocalIntent::Exit(Silent)` exits silently & clears history while `Wake` preserves recent history.
    #[tokio::test]
    async fn local_intent_exit_silent_clears_history_while_wake_preserves_recent_history() {
        let shell = Arc::new(RecordingShell::always_ok());
        let orch = test_orchestrator(shell.clone()).await;
        let config = orch.config.get().await;

        // Seed recent history and verify Wake preserves it.
        orch.agent
            .record_partial_turn("今天天气怎么样", "今天北京晴朗。")
            .await;
        orch.agent.reset_session("wake keyword").await;
        assert_eq!(
            orch.agent.history_len_for_test().await,
            1,
            "Wake reset must preserve recent history for barge-in follow-up"
        );

        // Now issue a Silent exit command: must speak nothing and clear history.
        let before_cmds = shell.command_count();
        let mut empty_count = 0u32;
        let mut ctx = LocalIntentContext {
            is_followup: true,
            answered_turns: 1,
            consecutive_empty_or_noise: &mut empty_count,
            max_empty_retries: 2,
            allow_wake_ack: true,
            enter_prompt: WAKE_ACK_PROMPT,
            exit_prompt: EXIT_FAREWELL_PROMPT,
            prefer_exit_earcon: false,
            speak_exit_on_noise_exhausted: false,
        };
        let outcome =
            handle_local_intent("闭嘴", &config, &orch.audio, &orch.agent, &mut ctx).await;

        assert_eq!(outcome, LocalIntentOutcome::EndSession);
        assert_eq!(
            shell.command_count(),
            before_cmds,
            "Exit(Silent) must not invoke TTS or any shell command"
        );
        assert_eq!(
            orch.agent.history_len_for_test().await,
            0,
            "Exit(Silent) must clear conversation history"
        );
        assert_eq!(
            orch.agent.reset_reason_for_test().await,
            "user exit command"
        );
    }

    /// (d) `TurnPipeline` mid-stream error handling:
    /// - After emitting >=1 chunk, LLM error speaks `"抱歉，后面的回答中断了"`.
    /// - With 0 chunks emitted, LLM error speaks `classify_turn_failure_prompt`.
    #[tokio::test]
    async fn turn_pipeline_speaks_wrapup_on_mid_stream_error_and_error_prompt_on_zero_chunks() {
        // Case 1: mid-stream error after 1 chunk was emitted.
        let shell = Arc::new(RecordingShell::always_ok());
        let orch = test_orchestrator(shell.clone()).await;
        let config = orch.config.get().await;

        let (delta_tx, delta_rx) = mpsc::unbounded_channel::<String>();
        let llm_fut = async move {
            delta_tx
                .send("今天北京天气晴朗，气温二十五度。".to_string())
                .unwrap();
            drop(delta_tx);
            Err::<AgentTurnResult, _>(anyhow::anyhow!("upstream SSE dropped mid-stream"))
        };

        let res = orch
            .pipeline()
            .execute_with_stream(
                TurnPipelineRequest {
                    config: &config,
                    turn_id: 101,
                    source: "test",
                    query: "今天天气怎么样",
                    asr_started_at: Instant::now(),
                    asr_ms: 120,
                    vad_silence_ms: None,
                    speech_voiced_ms: None,
                    pcm_bytes: None,
                    finish_mode: FinishMode::WaitForEnd,
                },
                llm_fut,
                delta_rx,
            )
            .await;

        assert!(res.is_err(), "mid-stream LLM error must propagate Err");
        let scripts = shell.scripts();
        assert!(
            scripts
                .iter()
                .any(|s| s.contains(MID_STREAM_INTERRUPTED_PROMPT)),
            "mid-stream error must speak wrap-up prompt, got scripts: {scripts:?}"
        );

        // Case 2: 0 chunks emitted -> speaks classified failure prompt, not wrap-up.
        let shell2 = Arc::new(RecordingShell::always_ok());
        let orch2 = test_orchestrator(shell2.clone()).await;
        let config2 = orch2.config.get().await;

        let (delta_tx2, delta_rx2) = mpsc::unbounded_channel::<String>();
        drop(delta_tx2);
        let llm_fut2 = async move { Err::<AgentTurnResult, _>(anyhow::anyhow!("大模型响应超时")) };

        let res2 = orch2
            .pipeline()
            .execute_with_stream(
                TurnPipelineRequest {
                    config: &config2,
                    turn_id: 102,
                    source: "test",
                    query: "讲个故事",
                    asr_started_at: Instant::now(),
                    asr_ms: 80,
                    vad_silence_ms: None,
                    speech_voiced_ms: None,
                    pcm_bytes: None,
                    finish_mode: FinishMode::WaitForEnd,
                },
                llm_fut2,
                delta_rx2,
            )
            .await;

        assert!(res2.is_err());
        let scripts2 = shell2.scripts();
        assert!(
            scripts2.iter().any(|s| s.contains("抱歉，大模型响应超时")),
            "zero-chunk error must speak classified failure prompt, got scripts: {scripts2:?}"
        );
        assert!(
            !scripts2
                .iter()
                .any(|s| s.contains(MID_STREAM_INTERRUPTED_PROMPT)),
            "zero-chunk error must not speak mid-stream wrap-up prompt"
        );
    }

    #[tokio::test]
    async fn turn_pipeline_records_full_stage_breakdown_and_perceived_e2e() {
        let shell = Arc::new(RecordingShell::always_ok());
        let orch = test_orchestrator(shell).await;
        let config = orch.config.get().await;

        let (delta_tx, delta_rx) = mpsc::unbounded_channel::<String>();
        let llm_fut = async move {
            delta_tx.send("你好，今天天气不错。".to_string()).unwrap();
            drop(delta_tx);
            Ok(AgentTurnResult {
                text: "你好，今天天气不错。".to_string(),
                should_end: false,
                end_reason: String::new(),
                tool_calls: 2,
            })
        };

        let res = orch
            .pipeline()
            .execute_with_stream(
                TurnPipelineRequest {
                    config: &config,
                    turn_id: 202,
                    source: "oneshot",
                    query: "今天天气怎么样",
                    asr_started_at: Instant::now() - Duration::from_millis(250),
                    asr_ms: 250,
                    vad_silence_ms: Some(600),
                    speech_voiced_ms: Some(900),
                    pcm_bytes: Some(32_000),
                    finish_mode: FinishMode::WaitForEnd,
                },
                llm_fut,
                delta_rx,
            )
            .await;
        assert!(res.is_ok());

        let snap = orch
            .audio
            .telemetry
            .last_turn()
            .expect("pipeline must record TurnLatencySnapshot");
        assert_eq!(snap.turn_id, 202);
        assert_eq!(snap.source, "oneshot");
        assert_eq!(snap.vad_silence_ms, Some(600));
        assert_eq!(snap.speech_voiced_ms, Some(900));
        assert_eq!(snap.pcm_bytes, Some(32_000));
        assert_eq!(snap.asr_ms, 250);
        assert!(snap.llm_ttft_ms.is_some());
        assert!(snap.llm_first_sentence_ms.is_some());
        assert!(snap.tts_first_audio_ms.is_some());
        assert_eq!(snap.tool_calls, 2);
        assert_eq!(snap.chunks, 1);
        let e2e = snap.e2e_first_audio_ms.expect("e2e_first_audio_ms must be recorded");
        assert!(e2e >= 240);
        assert_eq!(snap.e2e_perceived_ms, Some(e2e + 600));
    }
}


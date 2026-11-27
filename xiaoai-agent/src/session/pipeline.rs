//! Unified streaming LLM -> sentence splitting -> TTS pipeline shared by `exclusive` and `dual` modes.

use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::agent::{classify_turn_failure_prompt, AgentRuntime, AgentTurnResult};
use crate::audio::text_prep::StreamingSentenceSplitter;
use crate::audio::{AudioCoordinator, FinishMode};
use crate::config::AppConfig;
use crate::runtime::telemetry::{AgentPhase, TurnLatencySnapshot};

pub const THINKING_EARCON_DELAY_MS: u64 = 1200;
pub const MID_STREAM_INTERRUPTED_PROMPT: &str = "抱歉，后面的回答中断了";

/// Tracks the in-flight `(user_query, partial_reply)` pair so wake barge-in can preserve
/// partial context via `AgentRuntime::record_partial_turn`.
#[derive(Clone, Default)]
pub struct InFlightTurn(Arc<Mutex<Option<(String, String)>>>);

impl InFlightTurn {
    pub fn begin(&self, user: &str) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = Some((user.to_string(), String::new()));
        }
    }

    pub fn append(&self, chunk: &str) {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.0.lock() {
            if let Some((_, ref mut ans)) = *guard {
                if !ans.is_empty() {
                    ans.push(' ');
                }
                ans.push_str(trimmed);
            }
        }
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = None;
        }
    }

    pub fn take(&self) -> Option<(String, String)> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }
}

/// Ensures an asynchronous thinking earcon task is aborted if the pipeline future is cancelled.
struct AbortOnDrop(Option<JoinHandle<()>>);

impl AbortOnDrop {
    fn new() -> Self {
        Self(None)
    }

    fn set(&mut self, handle: JoinHandle<()>) {
        self.abort_now();
        self.0 = Some(handle);
    }

    fn abort_now(&mut self) -> bool {
        if let Some(handle) = self.0.take() {
            if !handle.is_finished() {
                handle.abort();
                return true;
            }
        }
        false
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.abort_now();
    }
}

/// Parameters for executing a single streaming LLM + TTS turn.
pub struct TurnPipelineRequest<'a> {
    pub config: &'a AppConfig,
    pub turn_id: u64,
    pub query: &'a str,
    pub asr_started_at: Instant,
    pub asr_ms: u64,
    pub finish_mode: FinishMode,
}

/// Shared streaming LLM -> `StreamingSentenceSplitter` -> `AudioCoordinator::speak_stream` pipeline.
pub struct TurnPipeline<'a> {
    agent: &'a Arc<AgentRuntime>,
    audio: &'a AudioCoordinator,
    in_flight: &'a InFlightTurn,
}

impl<'a> TurnPipeline<'a> {
    pub fn new(
        agent: &'a Arc<AgentRuntime>,
        audio: &'a AudioCoordinator,
        in_flight: &'a InFlightTurn,
    ) -> Self {
        Self {
            agent,
            audio,
            in_flight,
        }
    }

    /// Run a complete streaming LLM + TTS turn through `AgentRuntime::run_turn_stream`.
    pub async fn execute(&self, req: TurnPipelineRequest<'_>) -> anyhow::Result<AgentTurnResult> {
        let (delta_tx, delta_rx) = mpsc::unbounded_channel::<String>();
        let agent = self.agent.clone();
        let query_owned = req.query.to_string();
        let llm_fut = async move { agent.run_turn_stream(&query_owned, delta_tx).await };
        self.execute_with_stream(req, llm_fut, delta_rx).await
    }

    /// Core streaming pipeline parameterized over the LLM future and delta receiver so
    /// mid-stream error recovery and timeouts can be tested deterministically offline.
    pub async fn execute_with_stream<F>(
        &self,
        req: TurnPipelineRequest<'_>,
        llm_fut: F,
        mut delta_rx: mpsc::UnboundedReceiver<String>,
    ) -> anyhow::Result<AgentTurnResult>
    where
        F: Future<Output = anyhow::Result<AgentTurnResult>>,
    {
        self.in_flight.begin(req.query);

        let (tts_tx, tts_rx) = mpsc::channel::<String>(16);
        let llm_started_at = Instant::now();
        let first_token_ms = Arc::new(AtomicU64::new(u64::MAX));
        let first_audio_ms = Arc::new(AtomicU64::new(u64::MAX));
        let chunks_emitted = Arc::new(AtomicUsize::new(0));

        let llm_idle_timeout =
            Duration::from_secs_f64(req.config.llm.timeout_s.clamp(20.0, 45.0));
        let llm_hard_timeout =
            Duration::from_secs_f64((req.config.llm.timeout_s * 4.0).clamp(90.0, 180.0));

        let first_token_ref = first_token_ms.clone();
        let chunks_ref = chunks_emitted.clone();
        let audio_for_producer = self.audio.clone();
        let in_flight_ref = self.in_flight.clone();
        let turn_id = req.turn_id;
        let thinking_earcon = req.config.dual.thinking_earcon.clone();

        let producer_fut = async move {
            tokio::pin!(llm_fut);
            let llm_idle_deadline = sleep(llm_idle_timeout);
            tokio::pin!(llm_idle_deadline);
            let llm_hard_deadline = sleep(llm_hard_timeout);
            tokio::pin!(llm_hard_deadline);
            let thinking_timer = sleep(Duration::from_millis(THINKING_EARCON_DELAY_MS));
            tokio::pin!(thinking_timer);

            let mut splitter = StreamingSentenceSplitter::new();
            let mut llm_done: Option<anyhow::Result<AgentTurnResult>> = None;
            let mut delta_closed = false;
            let mut earcon_fired = false;
            let mut earcon_task = AbortOnDrop::new();

            loop {
                if llm_done.is_some() && delta_closed {
                    if let Some(tail) = splitter.finish() {
                        if chunks_ref.fetch_add(1, Ordering::Relaxed) == 0 {
                            if earcon_task.abort_now() {
                                audio_for_producer.device.abort_tts_preserving_music().await;
                            }
                            audio_for_producer.set_phase(AgentPhase::Speaking).await;
                        }
                        in_flight_ref.append(&tail);
                        let _ = tts_tx.send(tail).await;
                    }
                    break;
                }

                tokio::select! {
                    res = &mut llm_fut, if llm_done.is_none() => {
                        llm_done = Some(res);
                    }
                    _ = &mut llm_idle_deadline, if llm_done.is_none() => {
                        warn!(
                            turn_id,
                            idle_s = llm_idle_timeout.as_secs_f64(),
                            "大模型流式生成长时间无进展，提前止损"
                        );
                        llm_done = Some(Err(anyhow::anyhow!("大模型响应超时")));
                        delta_closed = true;
                    }
                    _ = &mut llm_hard_deadline, if llm_done.is_none() => {
                        warn!(
                            turn_id,
                            cap_s = llm_hard_timeout.as_secs_f64(),
                            "大模型整轮响应达到安全总时长上限，提前止损"
                        );
                        llm_done = Some(Err(anyhow::anyhow!("大模型整体响应超时")));
                        delta_closed = true;
                    }
                    _ = &mut thinking_timer, if !earcon_fired && chunks_ref.load(Ordering::Relaxed) == 0 && llm_done.is_none() => {
                        earcon_fired = true;
                        if let Some(ref earcon_path) = thinking_earcon {
                            if tokio::fs::metadata(earcon_path).await.is_ok() {
                                let earcon_audio = audio_for_producer.clone();
                                let path_str = earcon_path.to_string_lossy().into_owned();
                                earcon_task.set(tokio::spawn(async move {
                                    earcon_audio.play_earcon(&path_str).await;
                                }));
                            }
                        }
                    }
                    maybe_delta = delta_rx.recv(), if !delta_closed => {
                        match maybe_delta {
                            Some(delta) => {
                                llm_idle_deadline
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + llm_idle_timeout);
                                if first_token_ref.load(Ordering::Relaxed) == u64::MAX {
                                    let ttft = llm_started_at.elapsed().as_millis() as u64;
                                    let _ = first_token_ref.compare_exchange(
                                        u64::MAX,
                                        ttft,
                                        Ordering::AcqRel,
                                        Ordering::Relaxed,
                                    );
                                }
                                for sent in splitter.push_delta(&delta) {
                                    if chunks_ref.fetch_add(1, Ordering::Relaxed) == 0 {
                                        if earcon_task.abort_now() {
                                            audio_for_producer.device.abort_tts_preserving_music().await;
                                        }
                                        audio_for_producer.set_phase(AgentPhase::Speaking).await;
                                    }
                                    in_flight_ref.append(&sent);
                                    let _ = tts_tx.send(sent).await;
                                }
                            }
                            None => {
                                delta_closed = true;
                            }
                        }
                    }
                }
            }

            if earcon_task.abort_now() {
                audio_for_producer.device.abort_tts_preserving_music().await;
            }
            drop(tts_tx);
            (
                llm_done.expect("llm_done must be ready"),
                llm_started_at.elapsed().as_millis() as u64,
            )
        };

        let first_audio_ref = first_audio_ms.clone();
        let telemetry_for_tts = self.audio.telemetry.clone();
        let tts_cfg = &req.config.tts;
        let finish_mode = req.finish_mode;
        let asr_started_at = req.asr_started_at;
        let consumer_fut = async move {
            let tts_started_at = Instant::now();
            let res = self
                .audio
                .speak_stream(tts_cfg, tts_rx, finish_mode, move || {
                    telemetry_for_tts.set_phase(AgentPhase::Speaking);
                    let elapsed = asr_started_at.elapsed().as_millis() as u64;
                    let _ = first_audio_ref.compare_exchange(
                        u64::MAX,
                        elapsed,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );
                })
                .await;
            (res, tts_started_at.elapsed().as_millis() as u64)
        };

        let ((llm_res, llm_total_ms), (tts_res, tts_ms)) = tokio::join!(producer_fut, consumer_fut);
        self.in_flight.clear();
        if let Err(err) = tts_res {
            self.audio.tts_health.on_failure(&err);
        }

        let emitted = chunks_emitted.load(Ordering::Relaxed);
        match llm_res {
            Ok(reply) => {
                let ttft_raw = first_token_ms.load(Ordering::Relaxed);
                let llm_ttft_ms = (ttft_raw != u64::MAX).then_some(ttft_raw);
                let fa_raw = first_audio_ms.load(Ordering::Relaxed);
                let e2e_first_audio_ms = (fa_raw != u64::MAX).then_some(fa_raw);
                self.audio.telemetry.record_turn(TurnLatencySnapshot {
                    turn_id: req.turn_id,
                    asr_ms: req.asr_ms,
                    llm_ttft_ms,
                    llm_total_ms,
                    tts_ms,
                    e2e_first_audio_ms,
                });
                info!(
                    turn_id = req.turn_id,
                    asr_ms = req.asr_ms,
                    llm_ttft_ms = ?llm_ttft_ms,
                    llm_total_ms,
                    tts_ms,
                    e2e_first_audio_ms = ?e2e_first_audio_ms,
                    chunks = emitted,
                    reply_len = reply.text.chars().count(),
                    "TURN_METRICS"
                );
                Ok(reply)
            }
            Err(err) => {
                let tool_err = self.agent.take_last_tool_error().await;
                let tool_msg = tool_err.as_ref().map(|t| t.message.as_str()).unwrap_or("");
                warn!(
                    turn_id = req.turn_id,
                    tool = ?tool_err.as_ref().map(|t| t.tool),
                    tool_msg,
                    chunks = emitted,
                    error = %err,
                    "turn pipeline LLM failed"
                );
                self.audio.set_phase(AgentPhase::Speaking).await;
                if emitted > 0 {
                    // Mid-stream error recovery: avoid leaving the user hanging after partial speech.
                    if let Err(speak_err) = self.audio.device.speak(MID_STREAM_INTERRUPTED_PROMPT).await
                    {
                        warn!("failed to speak mid-stream interruption prompt: {speak_err:?}");
                    }
                } else {
                    let prompt = classify_turn_failure_prompt(tool_err.as_ref(), &err);
                    if let Err(speak_err) = self.audio.device.speak(prompt).await {
                        warn!("failed to speak service error prompt: {speak_err:?}");
                    }
                }
                Err(err.context("LLM failed after retries"))
            }
        }
    }
}

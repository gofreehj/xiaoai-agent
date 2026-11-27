use bytes::{Bytes, BytesMut};
use std::future::Future;
use std::time::Duration;

use tokio::time::{interval, sleep, Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::asr::{RealtimeAsrEvent, RealtimeAsrSession};
use crate::audio::CaptureOutcome;
use crate::config::CaptureConfig;
use crate::speech::{SpeechInput, SpeechTurn};
use crate::tts::PlaybackGate;
use crate::vad::{SpeechCollector, SpeechEvent};

/// 静音窗状态的轮询间隔。
///
/// 窗口内除了「丢弃 PCM」还要**把空闲超时按住**，而 PCM 什么时候断不由我们
/// 决定，所以用独立心跳，不靠收包驱动。
const QUIET_POLL: Duration = Duration::from_millis(250);

/// 静音窗的心跳：负责状态迁移的日志，以及在窗口内持续顺延空闲超时。
///
/// 返回「现在是否仍在窗口内」。`muted` 由调用方持有，用来识别迁移边沿——
/// 只有边沿才打日志，否则每 250ms 一条会把设备日志刷满。
fn poll_quiet(
    playback: &PlaybackGate,
    muted: &mut bool,
    dropped_bytes: &mut usize,
    turn_id: u64,
) -> bool {
    let now_muted = playback.mic_muted();
    if now_muted != *muted {
        if now_muted {
            info!(
                turn_id,
                dropped_bytes = *dropped_bytes,
                remaining_ms = playback
                    .mic_quiet_remaining()
                    .map(|left| left.as_millis() as u64)
                    .unwrap_or(0),
                "CAPTURE_MUTED 外部播放中：丢弃采集音频（回声中混着答案本身）"
            );
        } else {
            info!(
                turn_id,
                dropped_bytes = *dropped_bytes,
                "CAPTURE_UNMUTED 播放结束，恢复采集"
            );
        }
        *muted = now_muted;
        *dropped_bytes = 0;
    }
    now_muted
}

pub async fn record_utterance<F, Fut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    playback: &PlaybackGate,
    turn: &mut SpeechTurn,
    on_speech_start: F,
) -> anyhow::Result<Bytes>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = ()> + Send,
{
    match record_utterance_outcome(config, idle_timeout, playback, turn, on_speech_start).await? {
        CaptureOutcome::Utterance(pcm) => Ok(pcm),
        CaptureOutcome::IdleEnd => {
            anyhow::bail!("mipns ended the stream before an utterance was captured")
        }
    }
}

#[allow(dead_code)]
pub async fn record_utterance_outcome<F, Fut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    playback: &PlaybackGate,
    turn: &mut SpeechTurn,
    on_speech_start: F,
) -> anyhow::Result<CaptureOutcome>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = ()> + Send,
{
    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let mut collector = SpeechCollector::new(&config);
    let mut speech_started = false;
    let mut speech_prefix: Option<Bytes> = None;
    let mut muted = false;
    let mut dropped_bytes = 0_usize;
    let mut quiet_tick = interval(QUIET_POLL);
    quiet_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    info!(
        turn_id = turn.id(),
        "CAPTURE_BACKEND backend=speech_usock endpoint=energy"
    );
    loop {
        tokio::select! {
            _ = quiet_tick.tick() => {
                let was_muted = muted;
                let now_muted = poll_quiet(playback, &mut muted, &mut dropped_bytes, turn.id());
                if now_muted && speech_started {
                    collector = SpeechCollector::new(&config);
                    speech_started = false;
                    speech_prefix = None;
                }
                // 进窗要按住空闲超时；出窗要重新起算，否则「已在静音里超时」的
                // 定时器会在解除的瞬间把会话判掉。
                if now_muted || was_muted {
                    idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                }
            }
            _ = &mut idle_timer, if !speech_started => {
                if playback.mic_muted() {
                    // 静音窗不烧超时。心跳一般已经顺延过；这里兜住两次心跳之间
                    // 的边沿，也避免「已过期的定时器」在解除瞬间立刻触发。
                    idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                    continue;
                }
                collector.log_noise_floor("timeout");
                let _ = turn.stop_capture().await;
                anyhow::bail!("timed out waiting for user speech");
            }
            input = turn.recv() => {
                match input {
                    Some(SpeechInput::Pcm(bytes)) => {
                        if playback.mic_muted() {
                            if speech_started {
                                collector = SpeechCollector::new(&config);
                                speech_started = false;
                                speech_prefix = None;
                            }
                            dropped_bytes += bytes.len();
                            continue;
                        }
                        for event in collector.push(&bytes) {
                            match event {
                                SpeechEvent::SpeechStart(prefix) => {
                                    speech_started = true;
                                    speech_prefix = Some(prefix);
                                    on_speech_start().await;
                                }
                                SpeechEvent::SpeechChunk(_) => {}
                                SpeechEvent::SpeechRejected => {
                                    speech_started = false;
                                    speech_prefix = None;
                                    idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                                }
                                SpeechEvent::Utterance(tail) => {
                                    turn.stop_capture().await?;
                                    return Ok(CaptureOutcome::Utterance(assemble_utterance(
                                        speech_prefix.take(),
                                        tail,
                                    )));
                                }
                            }
                        }
                    }
                    Some(SpeechInput::End) => {
                        if speech_started {
                            if let Some(tail) = collector.flush() {
                                return Ok(CaptureOutcome::Utterance(assemble_utterance(
                                    speech_prefix.take(),
                                    tail,
                                )));
                            }
                        }
                        return Ok(CaptureOutcome::IdleEnd);
                    }
                    Some(SpeechInput::Cancelled) => {
                        anyhow::bail!("mipns cancelled the speech stream");
                    }
                    None => anyhow::bail!("speech.usock service stopped during capture"),
                }
            }
        }
    }
}

pub async fn record_utterance_streaming<F, Fut, C, CFut, R, RFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    playback: &PlaybackGate,
    turn: &mut SpeechTurn,
    on_speech_start: F,
    on_audio_chunk: C,
    on_speech_rejected: R,
) -> anyhow::Result<Bytes>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = ()> + Send,
    C: Fn(Bytes) -> CFut + Send + Sync,
    CFut: Future<Output = anyhow::Result<()>> + Send,
    R: Fn() -> RFut + Send + Sync,
    RFut: Future<Output = anyhow::Result<()>> + Send,
{
    match record_utterance_streaming_outcome(
        config,
        idle_timeout,
        playback,
        turn,
        on_speech_start,
        on_audio_chunk,
        on_speech_rejected,
    )
    .await?
    {
        CaptureOutcome::Utterance(pcm) => Ok(pcm),
        CaptureOutcome::IdleEnd => {
            anyhow::bail!("mipns ended the stream before an utterance was captured")
        }
    }
}

#[allow(dead_code)]
pub async fn record_utterance_streaming_outcome<F, Fut, C, CFut, R, RFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    playback: &PlaybackGate,
    turn: &mut SpeechTurn,
    on_speech_start: F,
    on_audio_chunk: C,
    on_speech_rejected: R,
) -> anyhow::Result<CaptureOutcome>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = ()> + Send,
    C: Fn(Bytes) -> CFut + Send + Sync,
    CFut: Future<Output = anyhow::Result<()>> + Send,
    R: Fn() -> RFut + Send + Sync,
    RFut: Future<Output = anyhow::Result<()>> + Send,
{
    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let mut collector = SpeechCollector::new(&config);
    let mut speech_started = false;
    let mut speech_prefix: Option<Bytes> = None;
    let mut muted = false;
    let mut dropped_bytes = 0_usize;
    let mut quiet_tick = interval(QUIET_POLL);
    quiet_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    info!(
        turn_id = turn.id(),
        "CAPTURE_BACKEND backend=speech_usock endpoint=energy streaming=true"
    );
    loop {
        tokio::select! {
            _ = quiet_tick.tick() => {
                let was_muted = muted;
                let now_muted = poll_quiet(playback, &mut muted, &mut dropped_bytes, turn.id());
                if now_muted && speech_started {
                    collector = SpeechCollector::new(&config);
                    speech_started = false;
                    speech_prefix = None;
                    on_speech_rejected().await?;
                }
                // 进窗要按住空闲超时；出窗要重新起算，否则「已在静音里超时」的
                // 定时器会在解除的瞬间把会话判掉。
                if now_muted || was_muted {
                    idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                }
            }
            _ = &mut idle_timer, if !speech_started => {
                if playback.mic_muted() {
                    // 静音窗不烧超时。心跳一般已经顺延过；这里兜住两次心跳之间
                    // 的边沿，也避免「已过期的定时器」在解除瞬间立刻触发。
                    idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                    continue;
                }
                collector.log_noise_floor("timeout");
                let _ = turn.stop_capture().await;
                anyhow::bail!("timed out waiting for user speech");
            }
            input = turn.recv() => {
                match input {
                    Some(SpeechInput::Pcm(bytes)) => {
                        if playback.mic_muted() {
                            if speech_started {
                                collector = SpeechCollector::new(&config);
                                speech_started = false;
                                speech_prefix = None;
                                on_speech_rejected().await?;
                            }
                            dropped_bytes += bytes.len();
                            continue;
                        }
                        for event in collector.push(&bytes) {
                            match event {
                                SpeechEvent::SpeechStart(prefix) => {
                                    speech_started = true;
                                    speech_prefix = Some(prefix.clone());
                                    on_speech_start().await;
                                    on_audio_chunk(prefix).await?;
                                }
                                SpeechEvent::SpeechChunk(chunk) => {
                                    on_audio_chunk(chunk).await?;
                                }
                                SpeechEvent::SpeechRejected => {
                                    speech_started = false;
                                    speech_prefix = None;
                                    on_speech_rejected().await?;
                                    idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                                }
                                SpeechEvent::Utterance(tail) => {
                                    turn.stop_capture().await?;
                                    return Ok(CaptureOutcome::Utterance(assemble_utterance(
                                        speech_prefix.take(),
                                        tail,
                                    )));
                                }
                            }
                        }
                    }
                    Some(SpeechInput::End) => {
                        if speech_started {
                            if let Some(tail) = collector.flush() {
                                return Ok(CaptureOutcome::Utterance(assemble_utterance(
                                    speech_prefix.take(),
                                    tail,
                                )));
                            }
                            on_speech_rejected().await?;
                        }
                        return Ok(CaptureOutcome::IdleEnd);
                    }
                    Some(SpeechInput::Cancelled) => {
                        anyhow::bail!("mipns cancelled the speech stream");
                    }
                    None => anyhow::bail!("speech.usock service stopped during capture"),
                }
            }
        }
    }
}

/// Streams mipns PCM until the remote ASR service closes the utterance with
/// its own VAD. Local timing is only used before speech and as a hard fail-safe.
pub async fn record_utterance_server_vad_streaming<F, Fut, E, EFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    playback: &PlaybackGate,
    turn: &mut SpeechTurn,
    session: &mut RealtimeAsrSession,
    on_speech_start: F,
    on_speech_stop: E,
) -> anyhow::Result<String>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = ()> + Send,
    E: Fn() -> EFut + Send + Sync,
    EFut: Future<Output = ()> + Send,
{
    match record_utterance_server_vad_streaming_outcome(
        config,
        idle_timeout,
        playback,
        turn,
        session,
        on_speech_start,
        on_speech_stop,
    )
    .await?
    {
        Some(text) => Ok(text),
        None => anyhow::bail!("mipns ended the stream before server VAD detected speech"),
    }
}

#[allow(dead_code)]
pub async fn record_utterance_server_vad_streaming_outcome<F, Fut, E, EFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    playback: &PlaybackGate,
    turn: &mut SpeechTurn,
    session: &mut RealtimeAsrSession,
    on_speech_start: F,
    on_speech_stop: E,
) -> anyhow::Result<Option<String>>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = ()> + Send,
    E: Fn() -> EFut + Send + Sync,
    EFut: Future<Output = ()> + Send,
{
    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let max_timer = sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(max_timer);
    let transcript_timer = sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(transcript_timer);
    let appender = session.appender();
    let mut speech_started = false;
    let mut input_closed = false;
    let mut chunks = 0_u64;
    let mut muted = false;
    let mut dropped_bytes = 0_usize;
    let mut quiet_tick = interval(QUIET_POLL);
    quiet_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    info!(
        turn_id = turn.id(),
        "CAPTURE_BACKEND backend=speech_usock endpoint=server_vad streaming=true"
    );
    loop {
        tokio::select! {
            _ = quiet_tick.tick() => {
                let was_muted = muted;
                let now_muted = poll_quiet(playback, &mut muted, &mut dropped_bytes, turn.id());
                if now_muted && speech_started && !input_closed {
                    speech_started = false;
                    appender.clear().await?;
                }
                // 进窗要按住空闲超时；出窗要重新起算，否则「已在静音里超时」的
                // 定时器会在解除的瞬间把会话判掉。
                if now_muted || was_muted {
                    idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                }
            }
            _ = &mut idle_timer, if !speech_started => {
                if playback.mic_muted() {
                    // 静音窗不烧超时。心跳一般已经顺延过；这里兜住两次心跳之间
                    // 的边沿，也避免「已过期的定时器」在解除瞬间立刻触发。
                    idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                    continue;
                }
                let _ = turn.stop_capture().await;
                anyhow::bail!("timed out waiting for user speech");
            }
            _ = &mut max_timer, if speech_started && !input_closed => {
                warn!(max_utterance_s = config.max_utterance_s, "server VAD did not end utterance; committing at local safety limit");
                turn.stop_capture().await?;
                input_closed = true;
                transcript_timer.as_mut().reset(Instant::now() + idle_timeout);
                session.commit().await?;
                on_speech_stop().await;
            }
            _ = &mut transcript_timer, if input_closed => {
                anyhow::bail!("timed out waiting for Realtime ASR transcript");
            }
            event = session.next_event() => {
                match event? {
                    RealtimeAsrEvent::SpeechStarted { audio_start_ms } => {
                        info!(audio_start_ms, "ASR_VAD_SPEECH_STARTED");
                        if !speech_started {
                            speech_started = true;
                            max_timer.as_mut().reset(Instant::now() + max_utterance_duration(&config));
                            on_speech_start().await;
                        }
                    }
                    RealtimeAsrEvent::SpeechStopped { audio_end_ms } => {
                        info!(audio_end_ms, "ASR_VAD_SPEECH_STOPPED");
                        if !input_closed {
                            turn.stop_capture().await?;
                            input_closed = true;
                            transcript_timer.as_mut().reset(Instant::now() + idle_timeout);
                            on_speech_stop().await;
                        }
                    }
                    RealtimeAsrEvent::Transcript(text) => {
                        if !input_closed {
                            turn.stop_capture().await?;
                        }
                        return Ok(Some(text));
                    }
                }
            }
            input = turn.recv(), if !input_closed => {
                match input {
                    Some(SpeechInput::Pcm(bytes)) => {
                        if playback.mic_muted() {
                            if speech_started && !input_closed {
                                speech_started = false;
                                appender.clear().await?;
                            }
                            dropped_bytes += bytes.len();
                            continue;
                        }
                        chunks += 1;
                        if chunks == 1 {
                            debug!(bytes = bytes.len(), "SPEECH_USOCK_FIRST_ASR_CHUNK");
                        }
                        appender.append_pcm(bytes).await?;
                    }
                    Some(SpeechInput::End) => {
                        if !speech_started {
                            return Ok(None);
                        }
                        input_closed = true;
                        transcript_timer.as_mut().reset(Instant::now() + idle_timeout);
                        session.commit().await?;
                        on_speech_stop().await;
                    }
                    Some(SpeechInput::Cancelled) => {
                        anyhow::bail!("mipns cancelled the speech stream");
                    }
                    None => anyhow::bail!("speech.usock service stopped during capture"),
                }
            }
        }
    }
}

fn max_utterance_duration(config: &CaptureConfig) -> Duration {
    Duration::from_secs_f64(config.max_utterance_s.max(1.0))
}

/// 拼接 `SpeechEvent::SpeechStart(prefix)` 与 `SpeechEvent::Utterance(tail)`。
///
/// `SpeechCollector` 在触发 `SpeechStart` 时会调用 `utterance.split().freeze()` 把
/// pre-roll 和首字触发帧移交给 `SpeechStart(prefix)`，后续 `Utterance(tail)` 只包含
/// 触发帧之后的尾段。不拼回 `prefix` 会导致整句开头 350~400ms 被截断。
pub fn assemble_utterance(prefix: Option<Bytes>, tail: Bytes) -> Bytes {
    match prefix {
        Some(pre) if !pre.is_empty() => {
            let mut combined = BytesMut::with_capacity(pre.len() + tail.len());
            combined.extend_from_slice(&pre);
            combined.extend_from_slice(&tail);
            combined.freeze()
        }
        _ => tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CaptureConfig;
    use crate::speech::{SpeechInput, SpeechTurn, TestCommand};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    /// 100ms @ 16kHz mono S16
    const BLOCK_BYTES: usize = 3200;

    fn test_config() -> CaptureConfig {
        CaptureConfig {
            silence_ms: 200,
            min_speech_ms: 100,
            cooldown_s: 0.0,
            ..CaptureConfig::default()
        }
    }

    fn pcm_block(sample: i16) -> Bytes {
        Bytes::from(
            (0..1600)
                .flat_map(|_| sample.to_le_bytes())
                .collect::<Vec<u8>>(),
        )
    }

    async fn commands(recorder: &Arc<Mutex<Vec<TestCommand>>>) -> Vec<TestCommand> {
        recorder.lock().await.clone()
    }

    /// 采集超时必须把 stop_capture 发出去再返回错误——只返回错误不发命令会让
    /// mipns 侧的 stream 一直挂着，下一次唤醒绑不上新 stream。
    ///
    /// 注意：`record_utterance` 内部把上限钳到至少 1 秒（`capture.rs:23`），
    /// 所以这条测试实跑约 1s。CI 里可接受，暂时不为此引入 `tokio::time::pause()`
    /// （paused clock 会让「谁在等时间」变模糊）；等这类慢测试累计超过 5s 再优化。
    #[tokio::test]
    async fn idle_timeout_stops_capture_and_reports_timeout() {
        let (mut turn, _input_tx, recorder) = SpeechTurn::for_test(1);
        let err = record_utterance(
            test_config(),
            // record_utterance 内部会把上限抬到至少 1s
            Duration::from_secs(1),
            &PlaybackGate::default(),
            &mut turn,
            || async {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("timed out"), "错误信息应说明超时: {err}");
        assert_eq!(commands(&recorder).await, vec![TestCommand::StopCapture]);
    }

    /// 静音窗内进来的 PCM 一条都不能喂 VAD：回声会被判成「用户开口」，一路录到
    /// `max_utterance` 再送 ASR，于是设备把自己的答案当成新问题（真机 14.7s 那次）。
    #[tokio::test]
    async fn muted_audio_never_reaches_the_collector() {
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(6);
        let gate = Arc::new(PlaybackGate::default());
        let task_gate = gate.clone();
        let handle = tokio::spawn(async move {
            record_utterance(
                test_config(),
                Duration::from_secs(30),
                &task_gate,
                &mut turn,
                || async {},
            )
            .await
        });

        // 静音窗内送「一次性说得完」的量（4 有声 + 3 静音）：没被丢掉的话这一轮
        // 早就该返回了。音量取 20000，好在返回的 PCM 里认出来。
        gate.mute_mic_for(Duration::from_secs(5));
        for _ in 0..4 {
            input_tx
                .send(SpeechInput::Pcm(pcm_block(20_000)))
                .await
                .unwrap();
        }
        for _ in 0..3 {
            input_tx.send(SpeechInput::Pcm(pcm_block(0))).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!handle.is_finished(), "静音窗内不许完成采集");

        gate.begin(); // 唤醒打断：播放器已被 killall，窗口跟着撤掉
        for _ in 0..4 {
            input_tx
                .send(SpeechInput::Pcm(pcm_block(1_000)))
                .await
                .unwrap();
        }
        for _ in 0..3 {
            input_tx.send(SpeechInput::Pcm(pcm_block(0))).await.unwrap();
        }

        let pcm = handle.await.unwrap().expect("窗口结束后应能正常采集");
        assert!(pcm.len() >= 4 * BLOCK_BYTES);
        let peak = pcm
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]).unsigned_abs())
            .max()
            .unwrap_or(0);
        assert!(
            peak <= 1_000,
            "返回的 PCM 里不该混进静音窗内的回声（peak={peak}）"
        );
        assert_eq!(commands(&recorder).await, vec![TestCommand::StopCapture]);
    }

    /// 静音窗内不许把空闲超时烧掉：默认 20s 比长回答短，播到一半判超时会直接
    /// 结束整段对话（用户只听到半句话，且没有下一轮）。
    #[tokio::test]
    async fn the_idle_timeout_is_paused_while_muted() {
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(7);
        let gate = Arc::new(PlaybackGate::default());
        let task_gate = gate.clone();
        let handle = tokio::spawn(async move {
            record_utterance(
                test_config(),
                Duration::from_secs(1),
                &task_gate,
                &mut turn,
                || async {},
            )
            .await
        });

        gate.mute_mic_for(Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(1_400)).await;
        assert!(!handle.is_finished(), "静音窗内不许判空闲超时");

        gate.begin();
        // 等一次心跳（250ms）把定时器重新起算：解除静音的瞬间不能靠运气。
        tokio::time::sleep(Duration::from_millis(300)).await;
        for _ in 0..4 {
            input_tx
                .send(SpeechInput::Pcm(pcm_block(10_000)))
                .await
                .unwrap();
        }
        for _ in 0..3 {
            input_tx.send(SpeechInput::Pcm(pcm_block(0))).await.unwrap();
        }

        let pcm = handle.await.unwrap().expect("解除静音后应能正常采集");
        assert!(pcm.len() >= 4 * BLOCK_BYTES);
        assert_eq!(commands(&recorder).await, vec![TestCommand::StopCapture]);
    }

    #[tokio::test]
    async fn end_before_speech_is_an_error_without_stop_command() {
        // 这里只固定「mipns 自己结束流时不补发 stop_capture」这条契约。
        // 用户体感的问题（说完话一片沉默、没有任何提示）在 main.rs 的错误分支里
        // 仍然存在，属于另一条待办；不要因为这条测试绿了就以为那条路径已经处理好。
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(2);
        input_tx.send(SpeechInput::End).await.unwrap();
        let err = record_utterance(
            test_config(),
            Duration::from_secs(30),
            &PlaybackGate::default(),
            &mut turn,
            || async {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("ended the stream"), "{err}");
        assert!(
            commands(&recorder).await.is_empty(),
            "mipns 自己结束了流，不需要我们补一条 stop_capture"
        );
    }

    #[tokio::test]
    async fn cancelled_stream_is_an_error() {
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(3);
        input_tx.send(SpeechInput::Cancelled).await.unwrap();
        let err = record_utterance(
            test_config(),
            Duration::from_secs(30),
            &PlaybackGate::default(),
            &mut turn,
            || async {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("cancelled"), "{err}");
        assert!(commands(&recorder).await.is_empty());
    }

    #[tokio::test]
    async fn voiced_then_silence_returns_pcm_and_stops_capture() {
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(4);
        let handle = tokio::spawn(async move {
            record_utterance(
                test_config(),
                Duration::from_secs(30),
                &PlaybackGate::default(),
                &mut turn,
                || async {},
            )
            .await
        });

        for _ in 0..4 {
            input_tx
                .send(SpeechInput::Pcm(pcm_block(10_000)))
                .await
                .unwrap();
        }
        // silence_ms=200 → 两个静音块就够收尾
        for _ in 0..3 {
            input_tx.send(SpeechInput::Pcm(pcm_block(0))).await.unwrap();
        }

        let pcm = handle.await.unwrap().expect("应当完成一次采集");
        assert!(
            pcm.len() >= 4 * BLOCK_BYTES,
            "至少应包含 4 个有声块（含 300ms pre-roll 会更多），实际 {} 字节",
            pcm.len()
        );
        assert_eq!(commands(&recorder).await, vec![TestCommand::StopCapture]);
    }

    /// 流式路径除了返回整段 PCM，还要把每个块实时转给 ASR：
    /// 只返回不发块，ASR 侧会一直等不到首包。
    #[tokio::test]
    async fn streaming_path_forwards_chunks_then_returns_utterance() {
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(5);
        let forwarded = Arc::new(Mutex::new(Vec::<usize>::new()));
        let collect = forwarded.clone();
        let handle = tokio::spawn(async move {
            record_utterance_streaming(
                test_config(),
                Duration::from_secs(30),
                &PlaybackGate::default(),
                &mut turn,
                || async {},
                move |chunk: Bytes| {
                    let collect = collect.clone();
                    async move {
                        collect.lock().await.push(chunk.len());
                        Ok(())
                    }
                },
                || async { Ok(()) },
            )
            .await
        });

        for _ in 0..4 {
            input_tx
                .send(SpeechInput::Pcm(pcm_block(10_000)))
                .await
                .unwrap();
        }
        for _ in 0..3 {
            input_tx.send(SpeechInput::Pcm(pcm_block(0))).await.unwrap();
        }

        let pcm = handle.await.unwrap().expect("应当完成一次采集");
        let forwarded = forwarded.lock().await.clone();
        assert!(
            !forwarded.is_empty(),
            "流式路径必须把块转给 ASR，否则 ASR 收不到音频"
        );
        assert!(
            forwarded.iter().all(|len| *len > 0),
            "转发的块不应为空: {forwarded:?}"
        );
        assert!(pcm.len() >= 4 * BLOCK_BYTES);
        assert_eq!(commands(&recorder).await, vec![TestCommand::StopCapture]);
    }

    /// 回归测试：`record_utterance` 必须保留 `SpeechStart(prefix)` 中的 pre-roll 与首字触发帧。
    /// 若丢弃 `SpeechStart(prefix)`，首块有声帧（振幅 15_000）会从返回结果中消失。
    #[tokio::test]
    async fn record_utterance_preserves_preroll_and_first_voiced_trigger_block() {
        let (mut turn, input_tx, _recorder) = SpeechTurn::for_test(8);
        let handle = tokio::spawn(async move {
            record_utterance(
                test_config(),
                Duration::from_secs(30),
                &PlaybackGate::default(),
                &mut turn,
                || async {},
            )
            .await
        });

        // 1 块低底噪 pre-roll (振幅 50) + 首块触发帧 (振幅 15_000) + 后续有声帧 (振幅 10_000) + 静音收尾
        input_tx.send(SpeechInput::Pcm(pcm_block(50))).await.unwrap();
        input_tx
            .send(SpeechInput::Pcm(pcm_block(15_000)))
            .await
            .unwrap();
        for _ in 0..3 {
            input_tx
                .send(SpeechInput::Pcm(pcm_block(10_000)))
                .await
                .unwrap();
        }
        for _ in 0..3 {
            input_tx.send(SpeechInput::Pcm(pcm_block(0))).await.unwrap();
        }

        let pcm = handle.await.unwrap().expect("应当完成采集");
        let has_pre_roll = pcm
            .chunks_exact(2)
            .any(|pair| i16::from_le_bytes([pair[0], pair[1]]) == 50);
        let has_first_trigger = pcm
            .chunks_exact(2)
            .any(|pair| i16::from_le_bytes([pair[0], pair[1]]) == 15_000);
        assert!(has_pre_roll, "返回的 PCM 必须保留 pre-roll 块 (sample=50)");
        assert!(
            has_first_trigger,
            "返回的 PCM 必须保留 SpeechStart 首字触发块 (sample=15_000)"
        );
    }

    #[tokio::test]
    async fn record_utterance_outcome_flushes_voiced_speech_on_end_and_returns_idle_end_on_silent_end()
    {
        // 1) Silent End -> CaptureOutcome::IdleEnd without StopCapture
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(9);
        input_tx.send(SpeechInput::End).await.unwrap();
        let outcome = record_utterance_outcome(
            test_config(),
            Duration::from_secs(30),
            &PlaybackGate::default(),
            &mut turn,
            || async {},
        )
        .await
        .expect("silent End should return Ok(IdleEnd)");
        assert!(matches!(outcome, CaptureOutcome::IdleEnd));
        assert!(commands(&recorder).await.is_empty());

        // 2) Voiced speech followed immediately by End (no trailing silence) -> flushed Utterance
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(10);
        let handle = tokio::spawn(async move {
            record_utterance_outcome(
                test_config(),
                Duration::from_secs(30),
                &PlaybackGate::default(),
                &mut turn,
                || async {},
            )
            .await
        });

        input_tx.send(SpeechInput::Pcm(pcm_block(50))).await.unwrap();
        input_tx
            .send(SpeechInput::Pcm(pcm_block(15_000)))
            .await
            .unwrap();
        for _ in 0..3 {
            input_tx
                .send(SpeechInput::Pcm(pcm_block(10_000)))
                .await
                .unwrap();
        }
        input_tx.send(SpeechInput::End).await.unwrap();

        let outcome = handle.await.unwrap().expect("voiced End should flush");
        let CaptureOutcome::Utterance(pcm) = outcome else {
            panic!("expected CaptureOutcome::Utterance on voiced End");
        };
        assert!(pcm.len() >= 4 * BLOCK_BYTES);
        let has_first_trigger = pcm
            .chunks_exact(2)
            .any(|pair| i16::from_le_bytes([pair[0], pair[1]]) == 15_000);
        assert!(
            has_first_trigger,
            "flushed utterance must include SpeechStart prefix"
        );
        assert!(commands(&recorder).await.is_empty());
    }

    #[tokio::test]
    async fn record_utterance_streaming_resets_and_rejects_when_muted_mid_speech() {
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(11);
        let gate = Arc::new(PlaybackGate::default());
        let task_gate = gate.clone();
        let rejected_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rejected_counter = rejected_count.clone();
        let started_notify = Arc::new(tokio::sync::Notify::new());
        let started_signal = started_notify.clone();

        let handle = tokio::spawn(async move {
            record_utterance_streaming(
                test_config(),
                Duration::from_secs(30),
                &task_gate,
                &mut turn,
                move || {
                    let started_signal = started_signal.clone();
                    async move {
                        started_signal.notify_one();
                    }
                },
                |_chunk: Bytes| async { Ok(()) },
                move || {
                    let rejected_counter = rejected_counter.clone();
                    async move {
                        rejected_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    }
                },
            )
            .await
        });

        // Trigger speech start
        input_tx
            .send(SpeechInput::Pcm(pcm_block(20_000)))
            .await
            .unwrap();
        started_notify.notified().await;

        // Mute mid-speech and send PCM while muted
        gate.mute_mic_for(Duration::from_secs(5));
        input_tx
            .send(SpeechInput::Pcm(pcm_block(20_000)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            rejected_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "muting mid-speech must call on_speech_rejected"
        );
        assert!(!handle.is_finished());

        // Unmute and deliver a clean utterance with a distinctive trigger block
        gate.begin();
        input_tx.send(SpeechInput::Pcm(pcm_block(50))).await.unwrap();
        input_tx
            .send(SpeechInput::Pcm(pcm_block(12_345)))
            .await
            .unwrap();
        for _ in 0..3 {
            input_tx
                .send(SpeechInput::Pcm(pcm_block(8_000)))
                .await
                .unwrap();
        }
        for _ in 0..3 {
            input_tx.send(SpeechInput::Pcm(pcm_block(0))).await.unwrap();
        }

        let pcm = handle
            .await
            .unwrap()
            .expect("should capture utterance after unmute");
        let has_trigger = pcm
            .chunks_exact(2)
            .any(|pair| i16::from_le_bytes([pair[0], pair[1]]) == 12_345);
        assert!(
            has_trigger,
            "streaming utterance must assemble SpeechStart prefix"
        );
        assert_eq!(commands(&recorder).await, vec![TestCommand::StopCapture]);
    }
}

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::agent::AgentRuntime;
use crate::asr::AsrClient;
use crate::audio::{AudioCoordinator, FinishMode};
use crate::config::{AppConfig, ConfigHandle, TtsProvider};
use crate::frontend::external_kws::{ExternalKwsEngine, ExternalKwsHandle, SessionTracker};
use crate::frontend::native_monitor::{drain_native_rx, wait_native_wake, NativeWakeMonitor};
use crate::frontend::BargeInSignal;
use crate::runtime::reload::{spawn_config_reload, ReloadGate};
use crate::runtime::telemetry::AgentPhase;
use crate::session::pipeline::{InFlightTurn, TurnPipeline, TurnPipelineRequest};
use crate::session::{
    handle_local_intent, play_optional_earcon, speak_prompt_wait, LocalIntentContext,
    LocalIntentOutcome,
};

const ONESHOT_PROBE_MS: u64 = 850;
const MAX_ENTER_PROMPT_DELAY_MS: u64 = 1250;
const MAX_WAKE_ASR_DELAY_MS: u64 = 850;
const MAX_CONSECUTIVE_NOISE_TURNS: u32 = 3;

/// 判断在 dual 模式下识别出的句首是否是原生小爱唤醒词（避免把用户对原生小爱的指令送给 LLM）。
pub fn is_native_xiaoai_utterance(raw_text: &str) -> bool {
    let cleaned = raw_text.trim_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '。' | '，' | '！' | '？' | '；' | '：' | '、' | '.' | ',' | '!' | '?' | ';' | ':' | '…'
            )
    });
    cleaned.starts_with("小爱同学")
}

/// 双助手共存守护进程 (mode=dual)
///
/// 架构设计：
/// 1. 原生小爱 ("小爱同学") 100% 保持不动，独占 speech.usock，听音乐、查天气、设闹钟、米家控制无任何影响；
/// 2. 外部唤醒由内置 [`ExternalKwsEngine`] 采集 ALSA PCM 并通过通用 UDP XPCM 协议发往任意外部 KWS 服务；
/// 3. 通过 [`NativeWakeMonitor`] 实时监听原生小爱唤醒信号，一旦用户改喊“小爱同学”，立刻打断全部会话与控制面任务；
/// 4. 通过统一的 `AudioCoordinator` + `TurnPipeline` 驱动本地意图、TTS、LED 灯效与连续长对话，并响应 SIGHUP 热重载。
#[allow(clippy::too_many_arguments)]
pub async fn run_dual_daemon(
    config_handle: ConfigHandle,
    config_path: PathBuf,
    asr: AsrClient,
    agent: Arc<AgentRuntime>,
    audio: AudioCoordinator,
    mut reload_rx: mpsc::Receiver<()>,
    reload_gate: Arc<ReloadGate>,
    native_monitor: NativeWakeMonitor,
) -> anyhow::Result<()> {
    let initial_config = config_handle.get().await;
    info!(
        endpoint = %initial_config.dual.kws.endpoint,
        device = %initial_config.dual.capture.device,
        native_interlock = initial_config.dual.native_interlock.enabled,
        "启动内置外部 KWS 双助手模式 (mode=dual)"
    );

    let mut kws = ExternalKwsEngine::spawn(&initial_config.dual).await?;
    let mut native_rx = native_monitor.subscribe();

    audio
        .device
        .blink_ready(
            initial_config.device.led_listening,
            Duration::from_millis(250),
        )
        .await;

    loop {
        let sig = tokio::select! {
            wake_res = kws.next_wake() => wake_res?,
            Some(()) = reload_rx.recv() => {
                spawn_config_reload(&config_handle, config_path.clone(), &agent, &reload_gate);
                continue;
            }
            _ = wait_native_wake(&mut native_rx) => {
                info!("待机期间检测到原生小爱唤醒，清理 Agent 音频输出");
                audio.yield_to_native_dialog(false).await;
                kws.drain_pending_wakes();
                continue;
            }
        };
        if sig != BargeInSignal::Wake {
            // 待机状态忽略 __CMD_EXIT__，避免环境人声说“退出/停止”误唤醒设备
            continue;
        }
        if native_monitor.is_blocked() {
            info!("原生小爱活跃或冷却保护中，忽略本次外部 KWS 唤醒");
            kws.drain_pending_wakes();
            continue;
        }
        drain_native_rx(&mut native_rx);

        let cur_config = config_handle.get().await;
        let mut tracker = SessionTracker::new();
        let in_flight = InFlightTurn::default();
        asr.prewarm();
        agent.prewarm().await;
        info!(
            session_id = tracker.session_id,
            "收到外部 KWS 唤醒事件，进入双助手对话接管！"
        );

        // 1. 优先通过 AudioCoordinator 推进令牌并暂停/闪避自建音乐与 AirPlay；
        //    若自建音乐未在播放，再补一条 mphelper pause 暂停原生播放器。
        audio.playback.begin();
        let music_interrupted = audio.music.interrupt_for_wake().await;
        let airplay_interrupted = audio.airplay.interrupt_for_wake().await;
        if !music_interrupted && !airplay_interrupted {
            let _ = audio
                .device
                .run_raw("mphelper pause", Duration::from_millis(250))
                .await;
            audio.device.abort_current_output().await;
        } else {
            audio.device.abort_tts_preserving_music().await;
        }

        let (_, _) = tokio::join!(
            audio.set_phase(AgentPhase::Listening),
            agent.reset_session("enter dual kws session"),
        );

        // 2. 用外层 select! 统一兜住 One-shot + 连续对话全阶段，确保任何时刻喊「小爱同学」都秒停秒让位。
        enum DualSessionOutcome {
            Completed(anyhow::Result<SessionExit>),
            NativeInterrupted,
        }

        let outcome = tokio::select! {
            res = async {
                let oneshot = probe_oneshot_or_prompt(
                    &mut kws,
                    &cur_config,
                    &asr,
                    &agent,
                    &audio,
                    &tracker,
                    &native_monitor,
                )
                .await?;
                match oneshot {
                    OneshotOutcome::ExitImmediately => {
                        info!(
                            session_id = tracker.session_id,
                            "One-shot 阶段收到本地结束指令，回到双助手就绪待机状态"
                        );
                        Ok(SessionExit::Normal)
                    }
                    OneshotOutcome::NativeYield => Ok(SessionExit::NativeYield),
                    OneshotOutcome::Query(initial_query) => {
                        run_conversation_session(
                            &mut kws,
                            &config_handle,
                            &asr,
                            &agent,
                            &audio,
                            &mut tracker,
                            &in_flight,
                            &native_monitor,
                            Some(initial_query),
                        )
                        .await
                    }
                    OneshotOutcome::NoSpeech => {
                        run_conversation_session(
                            &mut kws,
                            &config_handle,
                            &asr,
                            &agent,
                            &audio,
                            &mut tracker,
                            &in_flight,
                            &native_monitor,
                            None,
                        )
                        .await
                    }
                }
            } => DualSessionOutcome::Completed(res),
            _ = wait_native_wake(&mut native_rx) => DualSessionOutcome::NativeInterrupted,
        };

        let (yielded_to_native, session_res) = match outcome {
            DualSessionOutcome::NativeInterrupted => (true, Ok(())),
            DualSessionOutcome::Completed(Ok(SessionExit::NativeYield)) => (true, Ok(())),
            DualSessionOutcome::Completed(Ok(SessionExit::Normal)) => {
                (native_monitor.is_blocked(), Ok(()))
            }
            DualSessionOutcome::Completed(Err(err)) => (native_monitor.is_blocked(), Err(err)),
        };

        if yielded_to_native {
            // 先立即止声让位，再停流并排空残留唤醒，绝不发 shut_led / mphelper / 退出播报。
            audio.yield_to_native_dialog(true).await;
            let _ = in_flight.take();
            kws.stop_stream().await;
            kws.drain_pending_wakes();
            agent.reset_session("native xiaoai wake").await;
            info!(
                session_id = tracker.session_id,
                "已立即打断当前对话并静默让位给原生小爱"
            );
        } else {
            kws.stop_stream().await;
            kws.drain_pending_wakes();
            audio.release_dialog_focus().await;
            info!(
                session_id = tracker.session_id,
                "长对话会话已退出，回到双助手就绪待机状态"
            );
        }
        session_res?;
    }
}

struct PendingVoiceQuery {
    text: String,
    asr_started_at: Instant,
    asr_ms: u64,
    vad_stats: Option<crate::vad::UtteranceStats>,
    pcm_bytes: usize,
}

enum OneshotOutcome {
    Query(PendingVoiceQuery),
    NoSpeech,
    ExitImmediately,
    NativeYield,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionExit {
    Normal,
    NativeYield,
}

#[allow(clippy::too_many_arguments)]
async fn probe_oneshot_or_prompt(
    kws: &mut ExternalKwsHandle,
    cur_config: &AppConfig,
    asr: &AsrClient,
    agent: &Arc<AgentRuntime>,
    audio: &AudioCoordinator,
    tracker: &SessionTracker,
    native_monitor: &NativeWakeMonitor,
) -> anyhow::Result<OneshotOutcome> {
    if native_monitor.is_blocked() {
        return Ok(OneshotOutcome::NativeYield);
    }
    kws.start_oneshot_stream().await?;
    let max_timeout = Duration::from_secs(cur_config.dual.session_timeout_s.max(5));
    // One-shot 探测阶段按 40ms 包粒度即时判音，缩短首帧滞后
    let mut probe_capture = cur_config.capture.clone();
    probe_capture.block_ms = probe_capture.block_ms.min(40);
    let capture_res = kws
        .capture_utterance_with_stats(
            &probe_capture,
            max_timeout,
            Some(Duration::from_millis(ONESHOT_PROBE_MS)),
        )
        .await;
    kws.stop_stream().await;

    if native_monitor.is_blocked() {
        return Ok(OneshotOutcome::NativeYield);
    }

    if let Ok(Some(captured)) = capture_res {
        let pcm_bytes = captured.pcm.len();
        let vad_stats = captured.stats;
        let utterance_captured_at = Instant::now();
        audio.set_phase(AgentPhase::Thinking).await;
        let text_opt =
            transcribe_pcm_bytes(asr, &captured.pcm, cur_config.capture.sample_rate).await;
        let asr_ms = utterance_captured_at.elapsed().as_millis() as u64;
        if let Some(raw_text) = text_opt {
            if is_native_xiaoai_utterance(&raw_text) {
                info!(
                    session_id = tracker.session_id,
                    raw = %raw_text,
                    "One-shot 识别到原生小爱唤醒词，静默让位"
                );
                native_monitor.notify_native_utterance();
                return Ok(OneshotOutcome::NativeYield);
            }
            let mut empty_count = 0u32;
            let mut ctx = LocalIntentContext {
                is_followup: false,
                answered_turns: 0,
                consecutive_empty_or_noise: &mut empty_count,
                max_empty_retries: 2,
                allow_wake_ack: false,
                enter_prompt: &cur_config.dual.enter_prompt,
                exit_prompt: &cur_config.dual.exit_prompt,
                prefer_exit_earcon: true,
                speak_exit_on_noise_exhausted: false,
            };
            match handle_local_intent(&raw_text, cur_config, audio, agent, &mut ctx).await {
                LocalIntentOutcome::Chat(cleaned) => {
                    info!(
                        session_id = tracker.session_id,
                        raw = %raw_text,
                        query = %cleaned,
                        asr_ms,
                        vad_silence_ms = vad_stats.silence_tail_ms,
                        "One-shot 连说命中，跳过欢迎语直接回答"
                    );
                    return Ok(OneshotOutcome::Query(PendingVoiceQuery {
                        text: cleaned,
                        asr_started_at: utterance_captured_at,
                        asr_ms,
                        vad_stats: Some(vad_stats),
                        pcm_bytes,
                    }));
                }
                LocalIntentOutcome::EndSession | LocalIntentOutcome::VolumeHandled => {
                    return Ok(OneshotOutcome::ExitImmediately);
                }
                LocalIntentOutcome::ContinueListening => {}
            }
        }
        // 源头止损：已经录制过说话并走完 ASR，即使未得到有效文本也绝不事后补播欢迎语
        info!(
            session_id = tracker.session_id,
            elapsed_ms = tracker.wake_at.elapsed().as_millis() as u64,
            "One-shot 已收音但未识别出有效问题，跳过入场欢迎语直接进入倾听"
        );
        return Ok(OneshotOutcome::NoSpeech);
    }

    let max_enter_delay = Duration::from_millis(MAX_ENTER_PROMPT_DELAY_MS);
    if !tracker.is_fresh_for_enter_prompt(max_enter_delay) {
        warn!(
            session_id = tracker.session_id,
            elapsed_ms = tracker.wake_at.elapsed().as_millis() as u64,
            "兜底拦截过期入场欢迎语，跳过欢迎语直接进入倾听"
        );
        return Ok(OneshotOutcome::NoSpeech);
    }
    if native_monitor.is_blocked() {
        return Ok(OneshotOutcome::NativeYield);
    }

    info!(
        session_id = tracker.session_id,
        prompt = %cur_config.dual.enter_prompt,
        "未检测到 One-shot 连说，播报入场欢迎语"
    );
    play_prompt(
        audio,
        cur_config,
        cur_config.dual.enter_earcon.as_deref(),
        &cur_config.dual.enter_prompt,
    )
    .await;
    Ok(OneshotOutcome::NoSpeech)
}

async fn transcribe_pcm_bytes(asr: &AsrClient, pcm: &[u8], sample_rate: u32) -> Option<String> {
    info!(bytes = pcm.len(), "正在请求语音识别 ASR...");
    match asr.transcribe_pcm(pcm, sample_rate).await {
        Ok(t) => {
            let trimmed = t.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Err(err) => {
            warn!(error = %err, "ASR 识别失败或无语音内容");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_conversation_session(
    kws: &mut ExternalKwsHandle,
    config_handle: &ConfigHandle,
    asr: &AsrClient,
    agent: &Arc<AgentRuntime>,
    audio: &AudioCoordinator,
    tracker: &mut SessionTracker,
    in_flight: &InFlightTurn,
    native_monitor: &NativeWakeMonitor,
    mut pending_query: Option<PendingVoiceQuery>,
) -> anyhow::Result<SessionExit> {
    let mut last_activity = Instant::now();
    let mut consecutive_noise_turns: u32 = 0;
    let mut answered_turns: u32 = 0;

    loop {
        if native_monitor.is_blocked() {
            return Ok(SessionExit::NativeYield);
        }
        let cur_config = config_handle.get().await;

        let (user_text, source, asr_started_at, asr_ms, vad_stats, pcm_bytes) =
            if let Some(q) = pending_query.take() {
                (
                    q.text,
                    "oneshot",
                    q.asr_started_at,
                    q.asr_ms,
                    q.vad_stats,
                    Some(q.pcm_bytes),
                )
            } else {
                let session_timeout = Duration::from_secs(cur_config.dual.session_timeout_s);
                let remaining = session_timeout.saturating_sub(last_activity.elapsed());

                if remaining.is_zero() {
                    if native_monitor.is_blocked() {
                        return Ok(SessionExit::NativeYield);
                    }
                    info!("连续对话超过超时时间，自动退出");
                    play_prompt(
                        audio,
                        &cur_config,
                        cur_config.dual.exit_earcon.as_deref(),
                        &cur_config.dual.exit_prompt,
                    )
                    .await;
                    break;
                }

                kws.start_stream().await?;
                audio.set_phase(AgentPhase::Listening).await;
                info!("麦克风已开启，等待用户提问 (免唤醒连续模式)...");

                let utterance_res = kws
                    .capture_utterance_with_stats(&cur_config.capture, remaining, None)
                    .await;
                kws.stop_stream().await;

                if native_monitor.is_blocked() {
                    return Ok(SessionExit::NativeYield);
                }

                let captured = match utterance_res {
                    Ok(Some(captured)) => captured,
                    Ok(None) => {
                        info!("当前轮次未检测到有效声音，检查会话超时");
                        continue;
                    }
                    Err(e) => return Err(e),
                };

                let pcm_bytes = captured.pcm.len();
                let vad_stats = captured.stats;
                let utterance_captured_at = Instant::now();
                audio.set_phase(AgentPhase::Thinking).await;
                let text_opt =
                    transcribe_pcm_bytes(asr, &captured.pcm, cur_config.capture.sample_rate).await;
                let asr_ms = utterance_captured_at.elapsed().as_millis() as u64;

                let raw_text = text_opt.unwrap_or_default();
                if is_native_xiaoai_utterance(&raw_text) {
                    info!(
                        session_id = tracker.session_id,
                        raw = %raw_text,
                        "连续对话中检测到原生小爱唤醒词，停止回答并静默让位"
                    );
                    native_monitor.notify_native_utterance();
                    return Ok(SessionExit::NativeYield);
                }
                let allow_wake_ack =
                    utterance_captured_at.elapsed() <= Duration::from_millis(MAX_WAKE_ASR_DELAY_MS);
                let mut ctx = LocalIntentContext {
                    is_followup: true,
                    answered_turns,
                    consecutive_empty_or_noise: &mut consecutive_noise_turns,
                    max_empty_retries: MAX_CONSECUTIVE_NOISE_TURNS,
                    allow_wake_ack,
                    enter_prompt: &cur_config.dual.enter_prompt,
                    exit_prompt: &cur_config.dual.exit_prompt,
                    prefer_exit_earcon: true,
                    speak_exit_on_noise_exhausted: true,
                };

                match handle_local_intent(&raw_text, &cur_config, audio, agent, &mut ctx).await {
                    LocalIntentOutcome::ContinueListening => {
                        tracker.touch();
                        last_activity = Instant::now();
                        continue;
                    }
                    LocalIntentOutcome::VolumeHandled => {
                        tracker.touch();
                        last_activity = Instant::now();
                        continue;
                    }
                    LocalIntentOutcome::EndSession => {
                        break;
                    }
                    LocalIntentOutcome::Chat(cleaned) => (
                        cleaned,
                        "followup",
                        utterance_captured_at,
                        asr_ms,
                        Some(vad_stats),
                        Some(pcm_bytes),
                    ),
                }
            };

        let turn_seq = tracker.begin_turn();
        audio.set_phase(AgentPhase::Thinking).await;
        info!(
            session_id = tracker.session_id,
            turn_seq,
            query = %user_text,
            "正在流式请求大模型并开启边生成边播报..."
        );

        let pipeline = TurnPipeline::new(agent, audio, in_flight);
        let req = TurnPipelineRequest {
            config: &cur_config,
            turn_id: turn_seq,
            source,
            query: &user_text,
            asr_started_at,
            asr_ms,
            vad_silence_ms: vad_stats.map(|s| s.silence_tail_ms),
            speech_voiced_ms: vad_stats.map(|s| s.voiced_ms),
            pcm_bytes,
            finish_mode: FinishMode::WaitForEnd,
        };

        let turn_outcome = tokio::select! {
            res = pipeline.execute(req) => DualTurnOutcome::Finished(res),
            barge = kws.wait_for_barge_in() => DualTurnOutcome::BargeIn(barge?),
        };

        match turn_outcome {
            DualTurnOutcome::Finished(Ok(reply)) => {
                answered_turns += 1;
                // 尾音与房间混响消退缓冲，避免刚播完就开麦回采
                sleep(Duration::from_millis(350)).await;
                tracker.touch();

                if cur_config.tts.provider != TtsProvider::Native {
                    audio.music.ensure_playing_after_external_tts().await;
                }

                if reply.should_end {
                    info!(
                        session_id = tracker.session_id,
                        "大模型请求结束会话 (end_conversation)，退出连续对话模式"
                    );
                    agent.reset_session("agent ended conversation").await;
                    break;
                }
                if audio.music.is_actively_playing().await {
                    info!(
                        session_id = tracker.session_id,
                        "音乐已开始播放，自动退出连续对话以防麦克风回采音乐声"
                    );
                    break;
                }
            }
            DualTurnOutcome::Finished(Err(err)) => {
                error!(
                    session_id = tracker.session_id,
                    turn_seq,
                    error = %err,
                    "本轮对话失败"
                );
                sleep(Duration::from_millis(350)).await;
                tracker.touch();
            }
            DualTurnOutcome::BargeIn(BargeInSignal::Wake) => {
                info!(
                    session_id = tracker.session_id,
                    turn_seq,
                    "思考/播报期间收到唤醒词，立即打断并重新聆听"
                );
                audio.interrupt_for_wake().await;
                if let Some((user_q, partial_ans)) = in_flight.take() {
                    agent.record_partial_turn(&user_q, &partial_ans).await;
                }
                sleep(Duration::from_millis(60)).await;
                audio.set_phase(AgentPhase::Listening).await;
                tracker.reset_wake_window();
                let oneshot = probe_oneshot_or_prompt(
                    kws,
                    &cur_config,
                    asr,
                    agent,
                    audio,
                    tracker,
                    native_monitor,
                )
                .await?;
                match oneshot {
                    OneshotOutcome::ExitImmediately => break,
                    OneshotOutcome::NativeYield => return Ok(SessionExit::NativeYield),
                    OneshotOutcome::Query(q) => pending_query = Some(q),
                    OneshotOutcome::NoSpeech => {}
                }
                last_activity = Instant::now();
                continue;
            }
            DualTurnOutcome::BargeIn(BargeInSignal::ExitCmd) => {
                info!(
                    session_id = tracker.session_id,
                    turn_seq,
                    "思考/播报期间收到退出指令，立即打断并退出连续对话"
                );
                audio.playback.begin();
                audio.device.abort_tts_preserving_music().await;
                sleep(Duration::from_millis(60)).await;
                if !native_monitor.is_blocked() {
                    play_prompt(
                        audio,
                        &cur_config,
                        cur_config.dual.exit_earcon.as_deref(),
                        &cur_config.dual.exit_prompt,
                    )
                    .await;
                }
                agent.reset_session("user exit command").await;
                break;
            }
        }
        // 从回复播报完毕开始重新计算静默超时
        last_activity = Instant::now();
    }

    Ok(SessionExit::Normal)
}

enum DualTurnOutcome {
    Finished(anyhow::Result<crate::agent::AgentTurnResult>),
    BargeIn(BargeInSignal),
}

async fn play_prompt(
    audio: &AudioCoordinator,
    config: &AppConfig,
    wav_path: Option<&Path>,
    fallback_text: &str,
) {
    audio.set_phase(AgentPhase::Speaking).await;
    if play_optional_earcon(audio, wav_path).await {
        sleep(Duration::from_millis(350)).await;
        return;
    }
    speak_prompt_wait(audio, config, fallback_text).await;
    sleep(Duration::from_millis(350)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{classify_turn_failure_prompt, ToolFailureInfo};
    use crate::airplay::AirPlayService;
    use crate::config::CaptureConfig;
    use crate::device::{Device, STOP_ACTIVE_TTS_SCRIPT};
    use crate::frontend::external_kws::{capture_dual_utterance, AudioChunk, STEP_SAMPLES};
    use crate::music::MusicService;
    use crate::runtime::telemetry::RuntimeTelemetry;
    use crate::shell::RecordingShell;
    use crate::tts::{
        classify_exit_command, is_noise_or_filler, is_wake_word_only, split_tts_sentences,
        strip_wake_prefix, PlaybackGate, StreamingSentenceSplitter, TtsHealth, TtsRouter,
    };
    use bytes::Bytes;

    fn is_exit_command(text: &str, exit_keywords: &[String]) -> bool {
        classify_exit_command(text, exit_keywords).is_some()
    }

    #[test]
    fn split_tts_sentences_keeps_first_chunk_snappy_and_merges_rest() {
        let reply = "谷歌由拉里·佩奇和谢尔盖·布林于1998年在斯坦福大学创立，最初只是一个更好的网页搜索排序算法；2004年上市后，它陆续推出Gmail、谷歌地图和安卓系统，逐步从搜索引擎扩展成覆盖广告、移动操作系统和云服务的科技巨头。2015年，公司重组为控股公司Alphabet，把搜索、YouTube等核心业务与无人驾驶、生命科学等前沿项目分开管理。近些年，它的重心转向人工智能，从深度学习到Gemini大模型都是重点方向。";
        let chunks = split_tts_sentences(reply);
        assert!(chunks.len() <= 3, "expected <=3 chunks, got {:?}", chunks);
        assert!(
            chunks[0].chars().count() <= 50,
            "first chunk too long: {}",
            chunks[0]
        );
        for ch in &chunks[1..] {
            assert!(ch.chars().count() <= 110, "chunk exceeded 110 chars: {}", ch);
        }
    }

    #[test]
    fn split_tts_sentences_strips_markdown_and_ignores_punctuation_only() {
        let chunks = split_tts_sentences("**标题**\n\"测试内容\"。...");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("标题") && chunks[0].contains("测试内容"));
        assert!(!chunks[0].contains('*') && !chunks[0].contains('"'));
    }

    #[test]
    fn streaming_splitter_emits_first_sentence_before_stream_ends() {
        let mut splitter = StreamingSentenceSplitter::new();
        assert!(splitter.push_delta("今天天气").is_empty());
        let first = splitter.push_delta("晴朗，最高气温二十五度。明");
        assert_eq!(first, vec!["今天天气晴朗，最高气温二十五度。"]);
        assert!(splitter.push_delta("天有小雨").is_empty());
        assert_eq!(splitter.finish(), Some("明天有小雨".to_string()));
    }

    #[test]
    fn wake_word_only_matches_doubao_variants() {
        assert!(is_wake_word_only("豆包豆包。"));
        assert!(is_wake_word_only(" 豆包！"));
        assert!(!is_wake_word_only("豆包，今天天气怎么样？"));
        assert!(!is_wake_word_only("几点了"));
    }

    #[test]
    fn strip_wake_prefix_extracts_oneshot_query() {
        assert_eq!(
            strip_wake_prefix("豆包豆包，今天天气怎么样？"),
            "今天天气怎么样？"
        );
        assert_eq!(strip_wake_prefix("豆包 几点了"), "几点了");
        assert_eq!(strip_wake_prefix("豆包豆包。"), "");
        assert_eq!(strip_wake_prefix("帮我定个闹钟"), "帮我定个闹钟");
    }

    #[test]
    fn noise_or_filler_filters_meaningless_utterances() {
        assert!(is_noise_or_filler(""));
        assert!(is_noise_or_filler("..."));
        assert!(is_noise_or_filler("嗯。"));
        assert!(is_noise_or_filler("啊啊"));
        assert!(!is_noise_or_filler("停"));
        assert!(!is_noise_or_filler("今天天气怎么样"));
    }

    fn make_chunk(sample: i16, sys_active: bool) -> AudioChunk {
        let mut out = Vec::with_capacity(STEP_SAMPLES * 2);
        for _ in 0..STEP_SAMPLES {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        AudioChunk {
            pcm: Bytes::from(out),
            sys_active,
        }
    }

    #[test]
    fn classify_turn_failure_prompt_distinguishes_tool_and_llm_errors() {
        let llm_err = anyhow::anyhow!("streaming LLM turn failed");
        let search_timeout = ToolFailureInfo {
            tool: "web_search",
            message: "request timed out".into(),
            is_timeout: true,
        };
        assert_eq!(
            classify_turn_failure_prompt(Some(&search_timeout), &llm_err),
            "抱歉，联网搜索响应超时，请稍后再试"
        );
        let weather_fail = ToolFailureInfo {
            tool: "get_weather",
            message: "HTTP 502".into(),
            is_timeout: false,
        };
        assert_eq!(
            classify_turn_failure_prompt(Some(&weather_fail), &llm_err),
            "抱歉，天气服务暂时不可用"
        );
        let timeout_llm = anyhow::anyhow!("大模型整体响应超时");
        assert_eq!(
            classify_turn_failure_prompt(None, &timeout_llm),
            "抱歉，大模型响应超时，请稍后重试"
        );
        assert_eq!(
            classify_turn_failure_prompt(None, &llm_err),
            "抱歉，大模型回答遇到问题，请稍后重试"
        );
    }

    #[tokio::test]
    async fn capture_protects_active_speech_past_idle_deadline() {
        let (tx, mut rx) = mpsc::channel(32);
        let cfg = CaptureConfig {
            sample_rate: 16_000,
            block_ms: 40,
            pre_roll_ms: 80,
            silence_ms: 120,
            min_speech_ms: 80,
            threshold: 0.1,
            cooldown_s: 0.0,
            max_utterance_s: 5.0,
            ..CaptureConfig::default()
        };

        let sender = tokio::spawn(async move {
            tx.send(make_chunk(50, false)).await.unwrap();
            sleep(Duration::from_millis(60)).await;
            for _ in 0..4 {
                tx.send(make_chunk(12_000, false)).await.unwrap();
                sleep(Duration::from_millis(35)).await;
            }
            for _ in 0..4 {
                tx.send(make_chunk(0, false)).await.unwrap();
                sleep(Duration::from_millis(15)).await;
            }
        });

        let captured = capture_dual_utterance(&mut rx, &cfg, Duration::from_millis(100), None)
            .await
            .unwrap();
        sender.await.unwrap();
        assert!(
            captured.is_some(),
            "用户已经在空闲超时前开口说话，绝不能半句截断并返回 None"
        );
    }

    #[test]
    fn stop_active_tts_script_preserves_http_music_miplayer() {
        assert!(!STOP_ACTIVE_TTS_SCRIPT.contains("killall -q -9 miplayer"));
        assert!(!STOP_ACTIVE_TTS_SCRIPT.contains("killall miplayer"));
        assert!(!STOP_ACTIVE_TTS_SCRIPT.contains("tts_safe_play.sh"));
        assert!(STOP_ACTIVE_TTS_SCRIPT.contains("tts_play.sh"));
        assert!(STOP_ACTIVE_TTS_SCRIPT.contains("*http://*|*https://*"));
        assert!(STOP_ACTIVE_TTS_SCRIPT.contains("killall -q -9 qplayer"));
    }

    #[test]
    fn is_exit_command_does_not_hijack_music_or_device_commands() {
        let kws: Vec<String> = ["退出", "再见", "停止", "关闭", "取消"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(!is_exit_command("停止播放音乐。", &kws));
        assert!(!is_exit_command("把客厅灯关闭", &kws));
        assert!(!is_exit_command("取消明天的日程", &kws));
        assert!(is_exit_command("退出吧。", &kws));
        assert!(is_exit_command("再见", &kws));
        assert!(is_exit_command("停止！", &kws));
    }

    #[tokio::test]
    async fn handle_music_voice_control_stops_playing_song_without_restore() {
        let mut cfg = AppConfig::default();
        cfg.music.enabled = true;
        cfg.agent.tools_enabled = false;
        cfg.airplay.enabled = false;
        let shell = Arc::new(RecordingShell::always_ok());
        let device = Device::with_runner(cfg.device.clone(), shell.clone());
        let handle = ConfigHandle::new(Arc::new(cfg.clone()));
        let music = Arc::new(MusicService::new(handle.clone(), device.clone()).unwrap());
        let agent = Arc::new(
            AgentRuntime::new(handle.clone(), device.clone(), music.clone())
                .await
                .unwrap(),
        );
        let airplay = AirPlayService::start(cfg.airplay.clone()).await.unwrap();
        let playback = Arc::new(PlaybackGate::default());
        let tts = TtsRouter::new(device.clone(), playback.clone());
        let audio = AudioCoordinator::new(
            device,
            cfg.device.clone(),
            playback,
            tts,
            music.clone(),
            airplay,
            RuntimeTelemetry::default(),
            Arc::new(TtsHealth::default()),
        );
        music
            .set_current_for_test("kw_228908", "晴天", "https://example.com/qingtian.flac")
            .await;
        assert!(music.interrupt_for_wake().await);

        let mut empty_count = 0u32;
        let mut ctx = LocalIntentContext {
            is_followup: true,
            answered_turns: 0,
            consecutive_empty_or_noise: &mut empty_count,
            max_empty_retries: 3,
            allow_wake_ack: true,
            enter_prompt: &cfg.dual.enter_prompt,
            exit_prompt: &cfg.dual.exit_prompt,
            prefer_exit_earcon: true,
            speak_exit_on_noise_exhausted: false,
        };
        let outcome = handle_local_intent("停止播放音乐", &cfg, &audio, &agent, &mut ctx).await;
        assert_eq!(outcome, LocalIntentOutcome::EndSession);
        assert!(!music.has_current().await);

        let before_resume = shell.count_matching("mphelper play");
        music.restore_after_interruption().await;
        assert_eq!(
            shell.count_matching("mphelper play"),
            before_resume,
            "停止音乐后绝不能被 restore_after_interruption 重新拉起播放"
        );
    }

    #[tokio::test]
    async fn capture_ignores_system_active_audio_until_cleared() {
        let (tx, mut rx) = mpsc::channel(32);
        let cfg = CaptureConfig {
            sample_rate: 16_000,
            block_ms: 40,
            pre_roll_ms: 80,
            silence_ms: 120,
            min_speech_ms: 80,
            threshold: 0.1,
            cooldown_s: 0.0,
            ..CaptureConfig::default()
        };

        for _ in 0..5 {
            tx.send(make_chunk(20_000, true)).await.unwrap();
        }
        tx.send(make_chunk(50, false)).await.unwrap();
        for _ in 0..3 {
            tx.send(make_chunk(12_000, false)).await.unwrap();
        }
        for _ in 0..4 {
            tx.send(make_chunk(0, false)).await.unwrap();
        }

        let captured = capture_dual_utterance(&mut rx, &cfg, Duration::from_secs(2), None)
            .await
            .unwrap()
            .expect("should capture user utterance after system playback clears");

        let peak = captured
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]).abs())
            .max()
            .unwrap_or(0);
        assert_eq!(
            peak, 12_000,
            "system 回声片段(20000)不应混入用户捕获样本，实际峰值为 {peak}"
        );
    }

    #[test]
    fn is_native_xiaoai_utterance_matches_only_native_wake_prefix() {
        assert!(is_native_xiaoai_utterance("小爱同学，定个闹钟"));
        assert!(is_native_xiaoai_utterance("  小爱同学！ "));
        assert!(is_native_xiaoai_utterance("小爱同学"));
        assert!(!is_native_xiaoai_utterance("豆包，小爱同学是什么"));
        assert!(!is_native_xiaoai_utterance("帮我问问小爱同学"));
        assert!(!is_native_xiaoai_utterance("今天天气怎么样"));
    }
}

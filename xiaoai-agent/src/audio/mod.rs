//! Audio capture, VAD, playback, and device volume contracts.

pub mod output_format;
pub mod stream_player;
pub mod text_prep;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::warn;

use crate::airplay::AirPlayService;
use crate::config::{DeviceConfig, TtsConfig};
use crate::device::Device;
use crate::music::MusicService;
use crate::runtime::telemetry::{AgentPhase, RuntimeTelemetry};
use crate::tts::{PlaybackGate, TtsHealth, TtsRouter};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishMode {
    DetachForQuietWindow,
    WaitForEnd,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum CaptureOutcome {
    Utterance(Bytes),
    IdleEnd,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeAction {
    Up,
    Down,
    Set(u8),
    Mute,
}

const QUIET_WINDOW_WAIT_CAP: Duration = Duration::from_secs(15);
const QUIET_WINDOW_POLL_STEP: Duration = Duration::from_millis(50);

/// 统一协调音频输出、播放令牌、LED 阶段灯效、音乐/AirPlay 焦点打断与音量控制。
#[allow(dead_code)]
#[derive(Clone)]
pub struct AudioCoordinator {
    pub device: Device,
    pub led: DeviceConfig,
    pub playback: Arc<PlaybackGate>,
    pub tts: TtsRouter,
    pub music: Arc<MusicService>,
    pub airplay: AirPlayService,
    pub telemetry: RuntimeTelemetry,
    pub tts_health: Arc<TtsHealth>,
    pub stream_player: Arc<stream_player::AplayStreamPlayer>,
}

#[allow(dead_code)]
impl AudioCoordinator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device: Device,
        led: DeviceConfig,
        playback: Arc<PlaybackGate>,
        tts: TtsRouter,
        music: Arc<MusicService>,
        airplay: AirPlayService,
        telemetry: RuntimeTelemetry,
        tts_health: Arc<TtsHealth>,
    ) -> Self {
        Self {
            device,
            led,
            playback,
            tts,
            music,
            airplay,
            telemetry,
            tts_health,
            stream_player: Arc::new(stream_player::AplayStreamPlayer::new(
                stream_player::StreamAudioSpec::default(),
            )),
        }
    }

    fn led_for_phase(&self, phase: AgentPhase) -> Option<u8> {
        match phase {
            AgentPhase::Idle => None,
            AgentPhase::Listening => Some(self.led.led_listening),
            AgentPhase::UserSpeaking => Some(self.led.led_user_speaking),
            AgentPhase::Thinking => Some(self.led.led_thinking),
            AgentPhase::Speaking => Some(self.led.led_speaking),
        }
    }

    /// 声明式切换运行阶段与 LED：更新 telemetry，关掉上一阶段灯效并点亮新阶段灯效；
    /// 切回 `Idle` 时统一熄灭全部 4 盏阶段灯。
    pub async fn set_phase(&self, phase: AgentPhase) {
        let prev = self.telemetry.phase();
        self.telemetry.set_phase(phase);

        if phase == AgentPhase::Idle {
            for id in [
                self.led.led_speaking,
                self.led.led_thinking,
                self.led.led_user_speaking,
                self.led.led_listening,
            ] {
                self.device.shut_led(id).await;
            }
            return;
        }

        let prev_led = self.led_for_phase(prev);
        let next_led = self.led_for_phase(phase);
        if prev_led != next_led {
            if let Some(old_id) = prev_led {
                self.device.shut_led(old_id).await;
            }
        }
        if let Some(new_id) = next_led {
            self.device.show_led(new_id).await;
        }
    }

    /// 唤醒打断：立即推进播放令牌、打断流式播放、音乐/AirPlay，并按是否保留暂停音乐选择精准掐断方式。
    pub async fn interrupt_for_wake(&self) {
        self.playback.begin();
        self.stream_player.interrupt().await;
        let music_interrupted = self.music.interrupt_for_wake().await;
        let airplay_interrupted = self.airplay.interrupt_for_wake().await;
        if !music_interrupted && !airplay_interrupted {
            self.device.abort_current_output().await;
        } else {
            self.device.abort_tts_preserving_music().await;
        }
    }

    /// 彻底停止全部音频输出（流式播放 + TTS + 音乐），并回到 `Idle`。
    pub async fn stop_all_output(&self) {
        self.playback.begin();
        self.stream_player.interrupt().await;
        let _ = self.music.stop().await;
        self.device.abort_current_output().await;
        self.set_phase(AgentPhase::Idle).await;
    }

    /// 若提示音文件存在则通过 `qplayer` 播报；缺失时静默返回 `false`，不触发任何 shell 命令。
    pub async fn play_earcon(&self, wav_path: &str) -> bool {
        if tokio::fs::metadata(wav_path).await.is_err() {
            return false;
        }
        let quoted = shell_words::quote(wav_path);
        let script = format!("qplayer {quoted} 0 >/dev/null 2>&1");
        if let Err(err) = self.device.run_raw(&script, Duration::from_secs(6)).await {
            warn!(path = wav_path, error = %err, "failed to play earcon");
        }
        true
    }

    /// 委托 `TtsRouter::speak_stream_with_mode` 执行流式播报。
    pub async fn speak_stream<F>(
        &self,
        tts_cfg: &TtsConfig,
        rx: mpsc::Receiver<String>,
        finish_mode: FinishMode,
        on_first_speak: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut() + Send,
    {
        self.tts
            .speak_stream_with_mode(tts_cfg, rx, &self.tts_health, finish_mode, on_first_speak)
            .await
    }

    /// 单段文本播报：包装为单元素 channel 走统一的 `speak_stream`。
    pub async fn speak_once(
        &self,
        tts_cfg: &TtsConfig,
        text: &str,
        finish_mode: FinishMode,
    ) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel(1);
        let _ = tx.send(text.to_string()).await;
        drop(tx);
        let telemetry = self.telemetry.clone();
        self.speak_stream(tts_cfg, rx, finish_mode, move || {
            telemetry.set_phase(AgentPhase::Speaking);
        })
        .await
    }

    /// 执行本地音量控制动作。
    pub async fn execute_volume(&self, action: VolumeAction) -> anyhow::Result<()> {
        match action {
            VolumeAction::Up => self.device.volume_up().await,
            VolumeAction::Down => self.device.volume_down().await,
            VolumeAction::Set(level) => self.device.volume_set(level).await,
            VolumeAction::Mute => self.device.volume_mute().await,
        }
    }

    /// 等待外部播放静音窗结束或令牌失效（最长 15s），确保 LED 在麦克风真正解除静音后才切回 `Listening`。
    pub async fn wait_quiet_window_settled(&self, token: u64) {
        let deadline = Instant::now() + QUIET_WINDOW_WAIT_CAP;
        loop {
            if !self.playback.is_active(token) || !self.playback.mic_muted() {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            let remaining = self
                .playback
                .mic_quiet_remaining()
                .unwrap_or(Duration::ZERO);
            let step = remaining
                .min(deadline - now)
                .min(QUIET_WINDOW_POLL_STEP);
            if step.is_zero() {
                return;
            }
            tokio::select! {
                _ = sleep(step) => {}
                _ = self.playback.wait_until_inactive(token) => return,
            }
        }
    }

    /// 释放对话焦点：切回 `Idle` 并恢复被打断的音乐与 AirPlay。
    pub async fn release_dialog_focus(&self) {
        self.set_phase(AgentPhase::Idle).await;
        self.music.restore_after_interruption().await;
        self.airplay.restore_after_interruption().await;
    }

    /// `dual` 模式下向原生小爱让位：
    /// 1. 立即推进播放令牌并清静音窗（让在飞合成静默退出）；
    /// 2. 仅更新 telemetry 为 `Idle`，**不发 `shut_led`**（避免熄灭原生小爱醒灯）；
    /// 3. 放弃并清理 Agent 自有音乐状态（绝不随后误拉起 `mphelper play`，duck 模式下还原音量）；
    /// 4. 按需清理 Agent 自己的外部 TTS (`xiaoai-tts-*`) / 提示音 (`qplayer`)，绝不发 `mphelper pause`。
    pub async fn yield_to_native_dialog(&self, cleanup_tts: bool) {
        self.playback.begin();
        self.stream_player.interrupt().await;
        self.telemetry.set_phase(AgentPhase::Idle);
        let had_music = self.music.yield_to_native().await;
        if cleanup_tts || had_music {
            self.device.abort_agent_owned_tts().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, ConfigHandle};
    use crate::shell::RecordingShell;

    async fn test_coordinator(shell: Arc<RecordingShell>) -> AudioCoordinator {
        let mut cfg = AppConfig::default();
        cfg.airplay.enabled = false;
        let handle = ConfigHandle::new(Arc::new(cfg.clone()));
        let device = Device::with_runner(cfg.device.clone(), shell);
        let playback = Arc::new(PlaybackGate::default());
        let tts = TtsRouter::new(device.clone(), playback.clone());
        let music = Arc::new(MusicService::new(handle, device.clone()).unwrap());
        let airplay = AirPlayService::start(cfg.airplay).await.unwrap();
        AudioCoordinator::new(
            device,
            cfg.device,
            playback,
            tts,
            music,
            airplay,
            RuntimeTelemetry::default(),
            Arc::new(TtsHealth::default()),
        )
    }

    #[tokio::test]
    async fn set_phase_transitions_leds_declaratively_and_cleans_up_on_idle() {
        let shell = Arc::new(RecordingShell::always_ok());
        let coord = test_coordinator(shell.clone()).await;

        coord.set_phase(AgentPhase::Listening).await;
        assert_eq!(coord.telemetry.phase(), AgentPhase::Listening);
        assert_eq!(shell.scripts(), vec!["/bin/show_led 3 >/dev/null 2>&1"]);

        coord.set_phase(AgentPhase::UserSpeaking).await;
        assert_eq!(coord.telemetry.phase(), AgentPhase::UserSpeaking);
        assert_eq!(
            shell.scripts(),
            vec![
                "/bin/show_led 3 >/dev/null 2>&1",
                "/bin/shut_led 3 >/dev/null 2>&1",
                "/bin/show_led 4 >/dev/null 2>&1",
            ]
        );

        coord.set_phase(AgentPhase::Thinking).await;
        assert_eq!(coord.telemetry.phase(), AgentPhase::Thinking);

        coord.set_phase(AgentPhase::Speaking).await;
        assert_eq!(coord.telemetry.phase(), AgentPhase::Speaking);

        let before_idle = shell.count_matching("/bin/shut_led");
        coord.set_phase(AgentPhase::Idle).await;
        assert_eq!(coord.telemetry.phase(), AgentPhase::Idle);
        assert_eq!(
            shell.count_matching("/bin/shut_led") - before_idle,
            4,
            "回到 Idle 必须统一熄灭全部 4 盏阶段灯"
        );
    }

    #[tokio::test]
    async fn interrupt_for_wake_uses_full_abort_without_music_and_preserves_music_when_playing() {
        // Case 1: no music playing -> abort_current_output
        let shell = Arc::new(RecordingShell::always_ok());
        let coord = test_coordinator(shell.clone()).await;
        let before_token = coord.playback.current();

        coord.interrupt_for_wake().await;

        assert_eq!(coord.playback.current(), before_token + 1);
        assert!(
            shell.count_matching("killall tts_play.sh miplayer") >= 1,
            "无音乐时应走全量 abort_current_output"
        );
        assert_eq!(
            shell.count_matching("/proc/[0-9]*"),
            0,
            "无音乐时不需要走 abort_tts_preserving_music"
        );

        // Case 2: music playing -> abort_tts_preserving_music
        let shell2 = Arc::new(RecordingShell::always_ok());
        let coord2 = test_coordinator(shell2.clone()).await;
        coord2
            .music
            .set_current_for_test("kw_1", "稻香", "https://example.com/1.mp3")
            .await;
        let before_token2 = coord2.playback.current();

        coord2.interrupt_for_wake().await;

        assert_eq!(coord2.playback.current(), before_token2 + 1);
        assert_eq!(
            shell2.count_matching("/proc/[0-9]*"),
            1,
            "有音乐被打断时必须走 abort_tts_preserving_music"
        );
        assert_eq!(
            shell2.count_matching("killall tts_play.sh miplayer"),
            0,
            "有音乐时严禁 killall miplayer"
        );
    }

    #[tokio::test]
    async fn stop_all_output_stops_music_aborts_audio_and_sets_idle() {
        let shell = Arc::new(RecordingShell::always_ok());
        let coord = test_coordinator(shell.clone()).await;
        coord
            .music
            .set_current_for_test("kw_2", "七里香", "https://example.com/2.mp3")
            .await;
        coord.set_phase(AgentPhase::Speaking).await;
        let before_token = coord.playback.current();

        coord.stop_all_output().await;

        assert_eq!(coord.playback.current(), before_token + 1);
        assert!(!coord.music.has_current().await);
        assert_eq!(coord.telemetry.phase(), AgentPhase::Idle);
        assert!(shell.count_matching("killall tts_play.sh miplayer") >= 1);
    }

    #[tokio::test]
    async fn play_earcon_skips_shell_when_file_missing_and_runs_qplayer_when_present() {
        let shell = Arc::new(RecordingShell::always_ok());
        let coord = test_coordinator(shell.clone()).await;

        let missing = coord.play_earcon("/tmp/definitely-does-not-exist-xiaoai-earcon.wav").await;
        assert!(!missing);
        assert_eq!(
            shell.command_count(),
            0,
            "提示音文件缺失时不得触发任何 shell 命令"
        );

        let existing_path = std::env::temp_dir().join(format!(
            "xiaoai-earcon-test-{}.wav",
            std::process::id()
        ));
        std::fs::write(&existing_path, b"RIFF").unwrap();

        let played = coord
            .play_earcon(existing_path.to_str().unwrap())
            .await;
        let _ = std::fs::remove_file(&existing_path);

        assert!(played);
        assert_eq!(shell.command_count(), 1);
        let script = shell.last_script().unwrap();
        assert!(script.starts_with("qplayer "), "{script}");
        assert!(script.ends_with(" 0 >/dev/null 2>&1"), "{script}");
    }

    #[tokio::test]
    async fn execute_volume_dispatches_all_actions() {
        let shell = Arc::new(RecordingShell::always_ok());
        let coord = test_coordinator(shell.clone()).await;

        coord.execute_volume(VolumeAction::Up).await.unwrap();
        coord.execute_volume(VolumeAction::Down).await.unwrap();
        coord.execute_volume(VolumeAction::Set(60)).await.unwrap();
        coord.execute_volume(VolumeAction::Mute).await.unwrap();

        assert_eq!(shell.count_matching("volume_up"), 1);
        assert_eq!(shell.count_matching("volume_down"), 1);
        assert_eq!(shell.count_matching("volume_set 60"), 1);
        assert_eq!(shell.count_matching("volume_set 0"), 1);
    }

    #[tokio::test]
    async fn wait_quiet_window_settled_returns_promptly_on_token_invalidation() {
        let shell = Arc::new(RecordingShell::always_ok());
        let coord = test_coordinator(shell).await;
        let token = coord.playback.current();
        coord.playback.mute_mic_for(Duration::from_secs(10));

        let gate = coord.playback.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(30)).await;
            gate.begin();
        });

        let started = Instant::now();
        coord.wait_quiet_window_settled(token).await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "令牌失效时必须立刻结束静音窗等待"
        );
    }

    #[tokio::test]
    async fn yield_to_native_dialog_never_touches_native_led_tts_or_mphelper() {
        let shell = Arc::new(RecordingShell::always_ok());
        let coord = test_coordinator(shell.clone()).await;
        coord
            .music
            .set_current_for_test("kw_y", "夜曲", "https://example.com/yq.mp3")
            .await;
        assert!(coord.music.interrupt_for_wake().await);
        coord.set_phase(AgentPhase::Speaking).await;
        let old_token = coord.playback.current();
        let before_scripts = shell.command_count();

        coord.yield_to_native_dialog(true).await;

        assert_eq!(coord.telemetry.phase(), AgentPhase::Idle);
        assert!(!coord.playback.is_active(old_token));
        assert!(!coord.music.has_current().await);

        let yield_scripts = &shell.scripts()[before_scripts..];
        assert!(
            !yield_scripts.iter().any(|s| s.contains("shut_led")),
            "让位绝不能熄灭原生 LED: {yield_scripts:?}"
        );
        assert!(
            !yield_scripts.iter().any(|s| s.contains("mphelper")),
            "让位绝不能调 mphelper pause/play: {yield_scripts:?}"
        );
        assert!(
            !yield_scripts.iter().any(|s| s.contains("tts_play.sh")),
            "让位绝不能杀原生 tts_play.sh: {yield_scripts:?}"
        );
        assert!(
            yield_scripts.iter().any(|s| s.contains("xiaoai-tts-")),
            "让位必须精准清理 Agent 外部 TTS 播放器: {yield_scripts:?}"
        );

        let before_restore = shell.count_matching("mphelper play");
        coord.music.restore_after_interruption().await;
        assert_eq!(
            shell.count_matching("mphelper play"),
            before_restore,
            "让位后绝不允许再自动恢复音乐"
        );
    }

    #[tokio::test]
    async fn yield_to_native_dialog_unducks_ducked_music() {
        let mut cfg = AppConfig::default();
        cfg.airplay.enabled = false;
        cfg.music.interruption.mode = "duck".to_string();
        let shell = Arc::new(RecordingShell::always_ok());
        let handle = ConfigHandle::new(Arc::new(cfg.clone()));
        let device = Device::with_runner(cfg.device.clone(), shell.clone());
        let playback = Arc::new(PlaybackGate::default());
        let tts = TtsRouter::new(device.clone(), playback.clone());
        let music = Arc::new(MusicService::new(handle, device.clone()).unwrap());
        let airplay = AirPlayService::start(cfg.airplay).await.unwrap();
        let coord = AudioCoordinator::new(
            device,
            cfg.device,
            playback,
            tts,
            music,
            airplay,
            RuntimeTelemetry::default(),
            Arc::new(TtsHealth::default()),
        );
        coord
            .music
            .set_current_for_test("kw_d", "稻香", "https://example.com/dx.mp3")
            .await;
        assert!(coord.music.interrupt_for_wake().await);
        let before = shell.count_matching("mysoftvol");

        coord.yield_to_native_dialog(false).await;
        assert!(
            shell.count_matching("mysoftvol") > before,
            "duck 模式下让位必须恢复 mysoftvol 音量，避免原生小爱被迫低音量播放"
        );
    }
}

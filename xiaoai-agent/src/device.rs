use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, warn};

use crate::config::DeviceConfig;
use crate::shell::{CommandResult, ShellRunner, SystemShell};

pub const STOP_ACTIVE_TTS_SCRIPT: &str = "for p in /proc/[0-9]*; do \
   [ -r \"$p/cmdline\" ] || continue; \
   cmd=$(tr '\\0' ' ' <\"$p/cmdline\" 2>/dev/null); \
   case \"$cmd\" in \
     *tts_play.sh*) kill -9 \"${p#/proc/}\" 2>/dev/null || true ;; \
     *miplayer*) \
       case \"$cmd\" in \
         *http://*|*https://*) ;; \
         *) kill -9 \"${p#/proc/}\" 2>/dev/null || true ;; \
       esac \
       ;; \
   esac; \
 done; \
 killall -q -9 qplayer 2>/dev/null || true; \
 mphelper pause >/dev/null 2>&1 || true";

#[derive(Clone)]
pub struct Device {
    config: DeviceConfig,
    /// 唯一的 shell 出口。生产是 `SystemShell`，测试换成记录型 fake，
    /// 于是「设备侧到底发了什么命令」变成可断言的事实。
    runner: Arc<dyn ShellRunner>,
}

impl Device {
    pub fn new(config: DeviceConfig) -> Self {
        Self::with_runner(config, Arc::new(SystemShell))
    }

    /// 换掉 shell 执行体，其余设备语义（模板渲染、退出码判定、LED 开关）不变。
    pub fn with_runner(config: DeviceConfig, runner: Arc<dyn ShellRunner>) -> Self {
        Self { config, runner }
    }

    pub async fn speak(&self, text: &str) -> anyhow::Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.run_template(&self.config.tts_command, &[("text", text)])
            .await
    }

    pub async fn play_url(&self, url: &str) -> anyhow::Result<()> {
        self.run_template(&self.config.play_url_command, &[("url", url)])
            .await
    }

    pub async fn stop_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.stop_audio_command).await
    }

    pub async fn pause_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.pause_audio_command).await
    }

    pub async fn resume_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.resume_audio_command).await
    }

    pub async fn duck_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.duck_audio_command).await
    }

    pub async fn unduck_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.unduck_audio_command).await
    }

    #[allow(dead_code)]
    pub async fn volume_up(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.volume_up_command).await
    }

    #[allow(dead_code)]
    pub async fn volume_down(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.volume_down_command).await
    }

    #[allow(dead_code)]
    pub async fn volume_set(&self, level: u8) -> anyhow::Result<()> {
        let clamped = level.min(100).to_string();
        self.run_template(&self.config.volume_set_command, &[("level", &clamped)])
            .await
    }

    #[allow(dead_code)]
    pub async fn volume_mute(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.volume_mute_command).await
    }

    pub async fn abort_current_output(&self) {
        if let Err(err) = self.run_script(&self.config.abort_command).await {
            warn!("failed to abort current device output: {err:?}");
        }
    }

    /// 精准掐掉 TTS 播放进程（`tts_play.sh` / `qplayer` / 本地文件 `miplayer`），
    /// 保留暂停中的 `http(s)://` 音乐流 `miplayer` 进程。
    pub async fn abort_tts_preserving_music(&self) {
        if let Err(err) = self
            .run_raw(STOP_ACTIVE_TTS_SCRIPT, Duration::from_secs(5))
            .await
        {
            warn!(error = %err, "唤醒打断时没能掐掉 TTS 播放进程");
        }
    }

    pub async fn show_led(&self, id: u8) {
        if !self.config.led_enabled || id == 0 {
            return;
        }
        if let Err(err) = self
            .run_script(&format!("/bin/show_led {id} >/dev/null 2>&1"))
            .await
        {
            warn!("failed to show LED {id}: {err:?}");
        }
    }

    pub async fn shut_led(&self, id: u8) {
        if !self.config.led_enabled || id == 0 {
            return;
        }
        if let Err(err) = self
            .run_script(&format!("/bin/shut_led {id} >/dev/null 2>&1"))
            .await
        {
            warn!("failed to shut LED {id}: {err:?}");
        }
    }

    pub async fn blink_ready(&self, id: u8, duration: Duration) {
        self.show_led(id).await;
        sleep(duration).await;
        self.shut_led(id).await;
    }

    async fn run_template(&self, template: &str, values: &[(&str, &str)]) -> anyhow::Result<()> {
        let mut script = template.to_string();
        for (key, value) in values {
            let quoted = shell_words::quote(value);
            script = script.replace(&format!("{{{key}}}"), quoted.as_ref());
        }
        self.run_script(&script).await
    }

    /// 直接执行一条脚本。
    ///
    /// 给「不属于设备语义、但同样必须能拦住」的命令用：`main.rs` 的
    /// `pnshelper` 追问、故障让位都属于这一类。绕开这里直接调
    /// `shell::run_shell` 会让那条路径重新变成不可测。
    pub async fn run_raw(&self, script: &str, limit: Duration) -> anyhow::Result<CommandResult> {
        // 不再 `map_err(|err| anyhow!("{err}"))` 压成字符串：`ShellRunner` 直接返回
        // `anyhow::Result`，`io::Error` 的 source chain 会一路带到日志里。
        self.runner.run(script, limit).await
    }

    /// 同 `run_raw`，但不会在 future 被 drop 时杀掉子进程。
    ///
    /// 只给「必须跑完」的脚本用（目前只有故障让位的 restore 序列）。
    pub async fn run_detached(
        &self,
        script: &str,
        limit: Duration,
    ) -> anyhow::Result<CommandResult> {
        self.runner.run_detached(script, limit).await
    }

    async fn run_script(&self, script: &str) -> anyhow::Result<()> {
        let limit = std::time::Duration::from_secs_f64(self.config.command_timeout_s.max(1.0));
        let result = self.run_raw(script, limit).await?;
        if result.exit_code != 0 {
            anyhow::bail!(
                "command failed exit={} stderr={}",
                result.exit_code,
                result.stderr.trim()
            );
        }
        debug!(
            "device command ok: {}",
            script.lines().next().unwrap_or(script)
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::RecordingShell;

    fn device_with(shell: Arc<RecordingShell>) -> Device {
        Device::with_runner(DeviceConfig::default(), shell)
    }

    #[tokio::test]
    async fn speak_renders_the_template_through_the_runner() {
        let shell = Arc::new(RecordingShell::always_ok());
        device_with(shell.clone()).speak("你好 世界").await.unwrap();
        let script = shell.last_script().expect("speak 必须走 shell");
        assert!(
            script.starts_with("/usr/sbin/tts_play.sh"),
            "模板没渲染: {script}"
        );
        assert!(
            script.contains("'你好 世界'"),
            "带空格的中文必须被 shell_words 引号化，否则 tts_play.sh 会当成多个参数: {script}"
        );
    }

    #[tokio::test]
    async fn empty_speech_does_not_touch_the_shell() {
        // 空回复不该触发 TTS：设备上每多一次 tts_play.sh 就多一次 pns 通知。
        let shell = Arc::new(RecordingShell::always_ok());
        device_with(shell.clone()).speak("   ").await.unwrap();
        assert_eq!(shell.command_count(), 0);
    }

    #[tokio::test]
    async fn non_zero_exit_becomes_an_error() {
        // 设备脚本用返回码表达失败，上层（run_session 的错误分支）靠这个 Err
        // 决定要不要提示用户，吞掉就等于静默失败。
        let shell = Arc::new(RecordingShell::exiting_with(3, "no such device"));
        let err = device_with(shell)
            .speak("hi")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exit=3"), "{err}");
        assert!(err.contains("no such device"), "{err}");
    }

    #[tokio::test]
    async fn led_commands_are_skipped_when_disabled() {
        let shell = Arc::new(RecordingShell::always_ok());
        let mut config = DeviceConfig::default();
        config.led_enabled = false;
        let device = Device::with_runner(config, shell.clone());
        device.show_led(3).await;
        device.shut_led(3).await;
        assert_eq!(
            shell.command_count(),
            0,
            "led_enabled=false 时不该有 show_led/shut_led 子进程"
        );
    }

    #[tokio::test]
    async fn led_commands_use_the_configured_ids() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = device_with(shell.clone());
        device.show_led(4).await;
        device.shut_led(5).await;
        assert_eq!(shell.count_matching("/bin/show_led 4"), 1);
        assert_eq!(shell.count_matching("/bin/shut_led 5"), 1);
    }

    #[tokio::test]
    async fn abort_tts_preserving_music_filters_http_streams() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = device_with(shell.clone());
        device.abort_tts_preserving_music().await;
        let script = shell.last_script().expect("must run stop script");
        assert!(script.contains("*http://*|*https://*"), "{script}");
        assert!(!script.contains("killall miplayer"), "{script}");
    }

    #[tokio::test]
    async fn volume_methods_render_commands_and_clamp_level() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = device_with(shell.clone());

        device.volume_up().await.unwrap();
        assert!(
            shell.last_script().unwrap().contains("mphelper volume_up"),
            "{:?}",
            shell.last_script()
        );

        device.volume_down().await.unwrap();
        assert!(
            shell
                .last_script()
                .unwrap()
                .contains("mphelper volume_down"),
            "{:?}",
            shell.last_script()
        );

        device.volume_set(42).await.unwrap();
        assert!(
            shell
                .last_script()
                .unwrap()
                .contains("mphelper volume_set 42"),
            "{:?}",
            shell.last_script()
        );

        device.volume_set(150).await.unwrap();
        assert!(
            shell
                .last_script()
                .unwrap()
                .contains("mphelper volume_set 100"),
            "超过 100 的音量必须被钳制到 100: {:?}",
            shell.last_script()
        );

        device.volume_mute().await.unwrap();
        assert!(
            shell
                .last_script()
                .unwrap()
                .contains("mphelper volume_set 0"),
            "{:?}",
            shell.last_script()
        );
    }
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device").finish_non_exhaustive()
    }
}

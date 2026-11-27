//! Shared SIGHUP configuration hot-reload lifecycle for both `exclusive` and `dual` modes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::agent::AgentRuntime;
use crate::config::{classify_config_change, AppConfig, ConfigChanges, ConfigHandle};

/// 把本次不生效的配置段（需重启 / 被拒绝）回填成旧值。
///
/// 这样 `ConfigHandle` 里始终是「当前真正在跑的配置」，热加载后读句柄不会读到
/// 一份「device 是新值、实际 Device 还是旧模板」或「mode 已变但进程仍在原模式」的假象。
#[allow(dead_code)]
pub fn keep_pending_sections(
    current: &AppConfig,
    next: &Arc<AppConfig>,
    changes: &ConfigChanges,
) -> AppConfig {
    let mut effective = (**next).clone();
    for section in changes.rejected.iter().chain(changes.needs_restart.iter()) {
        match *section {
            "runtime.speech_socket" => {
                effective.runtime.speech_socket = current.runtime.speech_socket.clone();
            }
            "runtime.mode" => {
                effective.runtime.mode = current.runtime.mode.clone();
            }
            "device" => effective.device = current.device.clone(),
            "asr" => effective.asr = current.asr.clone(),
            "airplay" => effective.airplay = current.airplay.clone(),
            "server" => effective.server = current.server.clone(),
            "dual.kws" => effective.dual.kws = current.dual.kws.clone(),
            "dual.capture" => effective.dual.capture = current.dual.capture.clone(),
            "dual.native_interlock" => {
                effective.dual.native_interlock = current.dual.native_interlock.clone();
            }
            other => error!(
                section = other,
                "未知的待生效配置段：没有回填逻辑，句柄里会是新值（与实际运行值不一致）"
            ),
        }
    }
    effective
}

/// 监听 SIGHUP，只负责「通知主循环该重载了」。
///
/// 真正的重载在主循环里做：那样配置切换不会和一轮对话抢同一个 await 点，
/// 也让 `apply_config_reload` 能脱离信号被单测直接调用。
#[allow(dead_code)]
pub fn spawn_sighup_listener(tx: mpsc::Sender<()>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::hangup()) {
                Ok(mut stream) => {
                    while stream.recv().await.is_some() {
                        if tx.send(()).await.is_err() {
                            break;
                        }
                    }
                }
                Err(err) => warn!(error = %err, "无法安装 SIGHUP 处理器，配置热加载不可用"),
            }
        }
        #[cfg(not(unix))]
        {
            drop(tx);
            warn!("SIGHUP 配置热加载只在 unix 上可用");
        }
    });
}

/// 重载去抖门：同一时刻只跑一个重载任务，期间的请求**合并**成「跑完再跑一轮」。
#[allow(dead_code)]
#[derive(Default)]
pub struct ReloadGate {
    running: AtomicBool,
    pending: AtomicBool,
}

#[allow(dead_code)]
impl ReloadGate {
    /// 抢占本轮执行权。`false` 表示已有任务在跑，本次请求已并入下一轮。
    pub fn begin(&self) -> bool {
        if self.running.swap(true, Ordering::AcqRel) {
            self.pending.store(true, Ordering::Release);
            return false;
        }
        true
    }

    /// 一轮结束：还有合并进来的请求就返回 `true`（需要再跑一轮）。
    pub fn finish_round(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    pub fn release(&self) {
        self.running.store(false, Ordering::Release);
    }
}

/// 用 RAII 复位 `running`，而不是手动 `store(false)`。
#[allow(dead_code)]
pub struct ReloadGuard(pub Arc<ReloadGate>);

impl Drop for ReloadGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// 把一次重载丢到后台跑，避免 MCP 重连阻塞主事件循环。
#[allow(dead_code)]
pub fn spawn_config_reload(
    config: &ConfigHandle,
    path: PathBuf,
    agent: &Arc<AgentRuntime>,
    gate: &Arc<ReloadGate>,
) {
    if !gate.begin() {
        info!("配置重载进行中，本次 SIGHUP 合并到下一轮");
        return;
    }
    let config = config.clone();
    let agent = agent.clone();
    let gate = gate.clone();
    tokio::spawn(async move {
        // 无论正常结束还是 panic 展开，都会把 running 复位。
        let _guard = ReloadGuard(gate.clone());
        loop {
            apply_config_reload(&config, &path, &agent).await;
            if !gate.finish_round() {
                break;
            }
            info!("上一轮重载期间又来了一次 SIGHUP，按最新文件内容再跑一轮");
        }
    });
}

/// 执行一次配置重载：读文件 → 分类 → 换句柄 → 重建工具与 MCP。
///
/// 任何一步失败都保留旧配置：一份写坏的 yaml 不能让音箱因为一次 SIGHUP 变哑。
#[allow(dead_code)]
pub async fn apply_config_reload(handle: &ConfigHandle, path: &Path, agent: &AgentRuntime) {
    let current = handle.get().await;
    let next = match AppConfig::load(path) {
        Ok(next) => Arc::new(next),
        Err(err) => {
            warn!(
                error = %err,
                path = %path.display(),
                "配置重载失败，继续使用旧配置"
            );
            return;
        }
    };

    let changes = classify_config_change(&current, &next);
    if !changes.rejected.is_empty() {
        warn!(
            rejected = ?changes.rejected,
            "以下配置段运行中无法切换，本次忽略（其余段照常生效）"
        );
    }
    if !changes.needs_restart.is_empty() {
        warn!(
            needs_restart = ?changes.needs_restart,
            "以下配置段要重启 start-agent.sh 才生效，本次沿用旧值"
        );
    }

    if !changes.should_apply() {
        if changes.is_noop() {
            info!("配置文件无变化，跳过重载");
        } else {
            info!("本次没有可热加载的段，句柄与工具服务器保持不变");
        }
        return;
    }

    let effective = keep_pending_sections(&current, &next, &changes);
    let previous = handle.replace(Arc::new(effective)).await;
    agent.reload().await;
    info!(
        applied = ?changes.applied,
        "配置已热加载（下一轮生效）"
    );
    drop(previous);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Device;
    use crate::music::MusicService;
    use crate::shell::RecordingShell;

    fn temp_config_path(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "xiaoai-runtime-reload-{tag}-{}-{stamp}.yaml",
            std::process::id()
        ))
    }

    /// 回归测试：`runtime.mode` 属于 `needs_restart`，`keep_pending_sections` 必须回填旧值，
    /// 否则热加载句柄会声称已切换模式而进程仍在原模式运行。
    #[test]
    fn keep_pending_sections_rolls_back_runtime_mode() {
        let current = AppConfig::default();
        assert_eq!(current.runtime.mode, "exclusive");

        let mut next_cfg = current.clone();
        next_cfg.runtime.mode = "dual".to_string();
        next_cfg.runtime.session_idle_timeout_s = 33.0;
        let next = Arc::new(next_cfg);

        let changes = classify_config_change(&current, &next);
        assert!(changes.needs_restart.contains(&"runtime.mode"));
        assert!(changes.applied.contains(&"runtime.session_idle_timeout_s"));

        let effective = keep_pending_sections(&current, &next, &changes);
        assert_eq!(
            effective.runtime.mode, "exclusive",
            "runtime.mode 需要重启生效，必须回填为旧值"
        );
        assert_eq!(
            effective.runtime.session_idle_timeout_s, 33.0,
            "热加载段应照常生效"
        );
    }

    #[tokio::test]
    async fn apply_config_reload_rolls_back_runtime_mode_while_applying_hot_changes() {
        let mut initial = AppConfig::default();
        initial.agent.tools_enabled = false;
        initial.airplay.enabled = false;
        initial.runtime.mode = "exclusive".to_string();

        let handle = ConfigHandle::new(Arc::new(initial.clone()));
        let device = Device::with_runner(
            initial.device.clone(),
            Arc::new(RecordingShell::always_ok()),
        );
        let music = Arc::new(MusicService::new(handle.clone(), device.clone()).unwrap());
        let agent = AgentRuntime::new(handle.clone(), device, music)
            .await
            .unwrap();

        let path = temp_config_path("mode-rollback");
        std::fs::write(
            &path,
            "runtime:\n  mode: dual\n  session_idle_timeout_s: 45\nagent:\n  tools_enabled: false\n",
        )
        .unwrap();

        apply_config_reload(&handle, &path, &agent).await;
        let _ = std::fs::remove_file(&path);

        let after = handle.get().await;
        assert_eq!(after.runtime.mode, "exclusive");
        assert_eq!(after.runtime.session_idle_timeout_s, 45.0);
    }

    #[test]
    fn reload_gate_coalesces_and_guard_releases_on_panic() {
        let gate = Arc::new(ReloadGate::default());
        assert!(gate.begin());
        assert!(!gate.begin());
        assert!(gate.finish_round());
        assert!(!gate.finish_round());

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ReloadGuard(gate.clone());
            panic!("simulated reload panic");
        }));
        assert!(outcome.is_err());
        assert!(gate.begin(), "panic 展开后 ReloadGuard 必须复位门状态");
    }

    #[test]
    fn keep_pending_sections_rolls_back_dual_kws_and_capture_while_keeping_hot_prompt() {
        let mut current = AppConfig::default();
        current.dual.kws.endpoint = "192.168.31.1:5005".into();
        let mut next = current.clone();
        next.dual.kws.endpoint = "10.0.0.2:6000".into();
        next.dual.capture.channels = 2;
        next.dual.native_interlock.cooldown_s = 12;
        next.dual.enter_prompt = "请吩咐".into();

        let next = Arc::new(next);
        let changes = classify_config_change(&current, &next);
        let effective = keep_pending_sections(&current, &next, &changes);
        assert_eq!(effective.dual.kws.endpoint, "192.168.31.1:5005");
        assert_eq!(effective.dual.capture.channels, 8);
        assert_eq!(effective.dual.native_interlock.cooldown_s, 5);
        assert_eq!(effective.dual.enter_prompt, "请吩咐");
    }
}

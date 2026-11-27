//! Fault-handling boundary and yield-to-native contract (0/3/4/5 exit codes).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info, warn};

use crate::device::Device;
use crate::shell::TIMEOUT_ERROR_PREFIX;
use crate::speech::SocketOwnership;

/// 连续失败到这个数就让位。
#[allow(dead_code)]
pub const YIELD_AFTER_CONSECUTIVE_FAILURES: u32 = 3;

/// `start-agent.sh stop` 的「前端一点没动」退出码（锁被占用等）。
///
/// 契约在 `start-agent.sh` 顶部：0 成功 / 3 未受影响 / 4 半哑 / 5 拆到一半。
/// 只有 3 允许继续运行——其余都意味着前端已经被动过，继续跑会表现为
/// 「LED 正常闪、麦克风不工作」。
#[allow(dead_code)]
pub const LAUNCHER_EXIT_UNTOUCHED: i32 = 3;

/// `restore_native_files` 之后还有两步（`start_native_mico` / `pns start`）。
/// 它们失败时 **inode 已经换回原生的了**，但 mico/pns 没跑起来——socket 在位、
/// 麦克风不工作。这是唯一一个「inode 事实说成功、脚本自述说失败」的格子，
/// 必须以脚本为准，所以它的判定要排在 `succeeded()` 之前。
#[allow(dead_code)]
pub const LAUNCHER_EXIT_HALF_DISMANTLED: i32 = 5;

/// 一次让位尝试的结果。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YieldOutcome {
    pub issued: bool,
    pub taken_over: bool,
    /// 脚本真实退出码（没跑起来 / 超时则为 None）。
    pub exit_code: Option<i32>,
    /// 命令超时（脚本可能仍停在还原中途）。
    pub timed_out: bool,
}

#[allow(dead_code)]
impl YieldOutcome {
    /// 让位是否达成：命令成功，或者事实上已经被接管走。
    ///
    /// 但 `exit 5`（脚本自述「原生服务没起来」）**压过 inode 事实**：那两格
    /// （`start_native_mico` / `pns start`）发生在 `rm` + `mv` **之后**，inode 已经
    /// 换回原生的了，但 mico/pns 没跑——socket 在位、麦克风不工作。脚本比 inode
    /// 知道得多，所以这一档以脚本为准。
    pub fn succeeded(&self) -> bool {
        if self.exit_code == Some(LAUNCHER_EXIT_HALF_DISMANTLED) {
            return false;
        }
        self.issued || self.taken_over
    }
}

/// `start-agent.sh` 的位置：优先 `XIAOAI_AGENT_HOME`，否则跟二进制同目录。
#[allow(dead_code)]
pub fn launcher_path() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("XIAOAI_AGENT_HOME") {
        let candidate = PathBuf::from(home).join("start-agent.sh");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("start-agent.sh");
    candidate.is_file().then_some(candidate)
}

/// 故障让位：通过 `run_detached` 调用 `start-agent.sh stop` 把麦克风还给原厂小爱。
#[allow(dead_code)]
pub async fn yield_to_native(
    device: &Device,
    launcher: Option<&Path>,
    ownership: &SocketOwnership,
    reason: &str,
) -> YieldOutcome {
    let Some(launcher) = launcher else {
        error!(
            reason,
            "找不到 start-agent.sh，无法让位；只能退出交给 watchdog 处理"
        );
        return YieldOutcome {
            issued: false,
            taken_over: ownership.taken_over(),
            timed_out: false,
            exit_code: None,
        };
    };
    let script = format!(
        "{} stop",
        shell_words::quote(&launcher.display().to_string())
    );
    warn!(
        reason,
        launcher = %launcher.display(),
        "故障让位：把麦克风还给原厂小爱"
    );
    let mut timed_out = false;
    let mut exit_code = None;
    let issued = match device.run_detached(&script, Duration::from_secs(180)).await {
        Ok(result) if result.exit_code == 0 => {
            info!("launcher stop 已执行，原厂前端应当已恢复");
            exit_code = Some(0);
            true
        }
        Ok(result) => {
            error!(
                exit = result.exit_code,
                stderr = result.stderr.trim(),
                "让位命令执行失败"
            );
            exit_code = Some(result.exit_code);
            false
        }
        Err(err) => {
            timed_out = err.to_string().contains(TIMEOUT_ERROR_PREFIX);
            error!(error = %err, timed_out, "让位命令没能执行");
            false
        }
    };

    let taken_over = ownership.taken_over();
    if taken_over && !issued {
        warn!(
            socket = %ownership.path().display(),
            "让位命令报失败，但 speech.usock 已被原生接管，按成功处理"
        );
    }
    if issued && !taken_over {
        let detail = if ownership.still_owned() {
            "让位命令报告成功，但 speech.usock 还是本进程绑的 inode：麦克风可能没交出去"
        } else {
            "让位命令报告成功，但无法确认 speech.usock 归属：让位结果存疑"
        };
        warn!(socket = %ownership.path().display(), "{detail}");
    }
    YieldOutcome {
        issued,
        taken_over,
        timed_out,
        exit_code,
    }
}

/// 连续失败达到阈值后的处置：让位成功返回 `Some(0)`，仅在退出码 3（未动前端）时
/// 返回 `None` 并清零计数继续运行，其余（4/5/超时/缺失）返回 `Some(1)`。
#[allow(dead_code)]
pub async fn yield_after_consecutive_failures(
    device: &Device,
    launcher: Option<&Path>,
    ownership: &SocketOwnership,
    failures: &Arc<AtomicU32>,
) -> Option<i32> {
    let outcome = yield_to_native(device, launcher, ownership, "consecutive failures").await;
    if outcome.succeeded() {
        return Some(0);
    }
    if outcome.timed_out {
        error!(
            "让位命令超时：脚本可能停在还原中途，判失败并退出（继续运行会表现为「一切正常但麦克风不工作」）"
        );
        return Some(1);
    }
    if outcome.exit_code == Some(LAUNCHER_EXIT_UNTOUCHED) {
        warn!("让位命令未执行（launcher 锁被占用等），前端未受影响，继续运行；连续失败计数清零");
        failures.store(0, Ordering::Relaxed);
        return None;
    }
    if outcome.exit_code == Some(LAUNCHER_EXIT_HALF_DISMANTLED) {
        error!(
            "让位命令报告原生服务未起来（exit=5）：inode 已换回原生，但 mico/pns 不可用，判失败退出"
        );
    } else {
        error!(
            exit = ?outcome.exit_code,
            "让位失败且前端可能已被拆开：判失败退出，避免「看起来一切正常但麦克风不工作」"
        );
    }
    Some(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeviceConfig;
    use crate::shell::RecordingShell;

    fn temp_sock_path(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "xiaoai-fault-{tag}-{}-{stamp}.usock",
            std::process::id()
        ))
    }

    fn still_owned_socket(tag: &str) -> SocketOwnership {
        use std::os::unix::fs::MetadataExt;
        let path = temp_sock_path(tag);
        std::fs::write(&path, b"agent").unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        SocketOwnership::for_test(path, inode)
    }

    fn taken_over_socket(tag: &str) -> SocketOwnership {
        use std::os::unix::fs::MetadataExt;
        let live = temp_sock_path(tag);
        let native = live.with_extension("native.usock");
        std::fs::write(&live, b"agent").unwrap();
        std::fs::write(&native, b"native").unwrap();
        let mine = std::fs::metadata(&live).unwrap().ino();
        std::fs::remove_file(&live).unwrap();
        std::fs::rename(&native, &live).unwrap();
        SocketOwnership::for_test(live, mine)
    }

    #[test]
    fn exit_code_5_beats_replaced_inode_in_yield_outcome() {
        let half = YieldOutcome {
            issued: false,
            taken_over: true,
            timed_out: false,
            exit_code: Some(LAUNCHER_EXIT_HALF_DISMANTLED),
        };
        assert!(!half.succeeded(), "exit 5 必须压过 inode 已换事实");

        let restored = YieldOutcome {
            exit_code: Some(0),
            ..half
        };
        assert!(restored.succeeded());
    }

    #[tokio::test]
    async fn consecutive_failures_exit_1_on_exit_5_with_replaced_inode() {
        let shell = Arc::new(RecordingShell::exiting_with(
            LAUNCHER_EXIT_HALF_DISMANTLED,
            "failed to restart native mico",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
        let taken = taken_over_socket("exit5-replaced");

        let code = yield_after_consecutive_failures(
            &device,
            Some(Path::new("/data/open-xiaoai/start-agent.sh")),
            &taken,
            &failures,
        )
        .await;

        assert_eq!(code, Some(1));
        let _ = std::fs::remove_file(taken.path());
    }

    #[tokio::test]
    async fn consecutive_failures_keep_running_only_on_exit_3() {
        let shell = Arc::new(RecordingShell::exiting_with(
            LAUNCHER_EXIT_UNTOUCHED,
            "launcher lock busy",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
        let owned = still_owned_socket("exit3-untouched");

        let code = yield_after_consecutive_failures(
            &device,
            Some(Path::new("/data/open-xiaoai/start-agent.sh")),
            &owned,
            &failures,
        )
        .await;

        assert_eq!(code, None);
        assert_eq!(failures.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_file(owned.path());
    }
}

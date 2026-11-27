//! Native XiaoAI wake interlock monitor for `dual` mode.
//!
//! Watches both optional push event streams (`event_command`, e.g. `ubus listen`)
//! and native log files (`syslog_path` + `instruction_log_path`) so saying
//! "小爱同学" immediately cancels active Agent work and suppresses conflicting
//! external KWS wakes until native XiaoAI finishes.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::config::NativeInterlockConfig;

const BROADCAST_CAP: usize = 16;
const DEBOUNCE_WINDOW: Duration = Duration::from_secs(1);
const MIN_EXTERNAL_COOLDOWN: Duration = Duration::from_secs(3);
const MAX_READ_BYTES_PER_POLL: usize = 256 * 1024;
const MAX_PENDING_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct NativeInterlockState {
    wake_patterns: Vec<String>,
    end_patterns: Vec<String>,
    instruction_patterns: Vec<String>,
    max_active: Duration,
    cooldown: Duration,
    active_until: Option<Instant>,
    hold_until: Option<Instant>,
    last_emit: Option<Instant>,
}

impl NativeInterlockState {
    pub fn from_config(cfg: &NativeInterlockConfig) -> Self {
        Self {
            wake_patterns: cfg
                .wake_patterns
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            end_patterns: cfg
                .end_patterns
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            instruction_patterns: cfg
                .instruction_patterns
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            max_active: Duration::from_secs(cfg.max_active_s.max(1)),
            cooldown: Duration::from_secs(cfg.cooldown_s),
            active_until: None,
            hold_until: None,
            last_emit: None,
        }
    }

    pub fn is_blocked(&self, now: Instant) -> bool {
        self.active_until.is_some_and(|t| now < t) || self.hold_until.is_some_and(|t| now < t)
    }

    fn debounce_allows(&self, now: Instant) -> bool {
        self.last_emit
            .is_none_or(|prev| now.saturating_duration_since(prev) >= DEBOUNCE_WINDOW)
    }

    fn extend_hold(&mut self, until: Instant) {
        self.hold_until = Some(match self.hold_until {
            Some(existing) if existing > until => existing,
            _ => until,
        });
    }

    /// Observe one line from syslog or `event_command`.
    /// Returns `true` when callers should emit a rising-edge wake interruption signal.
    pub fn observe_event_line(&mut self, line: &str, now: Instant) -> bool {
        if matches_any(line, &self.wake_patterns) {
            let was_active = self.active_until.is_some_and(|t| now < t);
            self.active_until = Some(now + self.max_active);
            if !was_active && self.debounce_allows(now) {
                self.last_emit = Some(now);
                return true;
            }
            return false;
        }
        if matches_any(line, &self.end_patterns) {
            self.active_until = None;
            if !self.cooldown.is_zero() {
                self.extend_hold(now + self.cooldown);
            }
            return false;
        }
        false
    }

    /// Observe one line from `instruction_log_path`.
    /// Extends the cooldown window and triggers a fallback wake if syslog missed it.
    pub fn observe_instruction_line(&mut self, line: &str, now: Instant) -> bool {
        if !matches_any(line, &self.instruction_patterns) {
            return false;
        }
        if !self.cooldown.is_zero() {
            self.extend_hold(now + self.cooldown);
        }
        let currently_active = self.active_until.is_some_and(|t| now < t);
        if !currently_active && self.debounce_allows(now) {
            self.last_emit = Some(now);
            return true;
        }
        false
    }

    /// Trigger interlock manually (e.g. ASR in `dual` mode recognized `"小爱同学..."`).
    pub fn trigger_external(&mut self, now: Instant) -> bool {
        let hold = self.cooldown.max(MIN_EXTERNAL_COOLDOWN);
        self.extend_hold(now + hold);
        self.last_emit = Some(now);
        true
    }
}

fn matches_any(line: &str, patterns: &[String]) -> bool {
    !patterns.is_empty() && patterns.iter().any(|p| line.contains(p.as_str()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

fn file_identity(meta: &std::fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        FileIdentity { dev: 0, ino: 0 }
    }
}

#[derive(Debug)]
struct LogTailer {
    candidates: Vec<PathBuf>,
    tracked: Option<(usize, FileIdentity, u64)>,
    pending: Vec<u8>,
}

impl LogTailer {
    fn new(candidates: Vec<PathBuf>, skip_existing: bool) -> Self {
        let mut tailer = Self {
            candidates,
            tracked: None,
            pending: Vec::new(),
        };
        if skip_existing {
            if let Some((idx, meta)) = tailer.find_existing() {
                tailer.tracked = Some((idx, file_identity(&meta), meta.len()));
            }
        }
        tailer
    }

    fn find_existing(&self) -> Option<(usize, std::fs::Metadata)> {
        if let Some((cur_idx, _, _)) = self.tracked {
            if let Some(path) = self.candidates.get(cur_idx) {
                if let Ok(meta) = std::fs::metadata(path) {
                    return Some((cur_idx, meta));
                }
            }
        }
        for (idx, path) in self.candidates.iter().enumerate() {
            if let Ok(meta) = std::fs::metadata(path) {
                return Some((idx, meta));
            }
        }
        None
    }

    fn poll_new_lines(&mut self) -> Vec<String> {
        let Some((idx, meta)) = self.find_existing() else {
            self.tracked = None;
            self.pending.clear();
            return Vec::new();
        };
        let ident = file_identity(&meta);
        let file_len = meta.len();
        let mut offset = match self.tracked {
            Some((prev_idx, prev_ident, prev_off)) if prev_idx == idx && prev_ident == ident => {
                if file_len < prev_off {
                    self.pending.clear();
                    0
                } else {
                    prev_off
                }
            }
            _ => {
                self.pending.clear();
                0
            }
        };

        if file_len == offset {
            self.tracked = Some((idx, ident, offset));
            return Vec::new();
        }

        let path = &self.candidates[idx];
        let Ok(mut file) = OpenOptions::new().read(true).open(path) else {
            self.tracked = None;
            self.pending.clear();
            return Vec::new();
        };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            self.tracked = None;
            self.pending.clear();
            return Vec::new();
        }

        let to_read = (file_len - offset).min(MAX_READ_BYTES_PER_POLL as u64) as usize;
        let mut buf = vec![0u8; to_read];
        let Ok(n) = file.read(&mut buf) else {
            return Vec::new();
        };
        if n == 0 {
            self.tracked = Some((idx, ident, offset));
            return Vec::new();
        }
        offset += n as u64;
        self.tracked = Some((idx, ident, offset));

        self.pending.extend_from_slice(&buf[..n]);
        if self.pending.len() > MAX_PENDING_LINE_BYTES {
            let drain_to = self.pending.len() - MAX_PENDING_LINE_BYTES;
            self.pending.drain(..drain_to);
        }

        let mut lines = Vec::new();
        let mut start = 0usize;
        while let Some(rel_pos) = self.pending[start..].iter().position(|&b| b == b'\n') {
            let end = start + rel_pos;
            let raw_line = &self.pending[start..end];
            let line = String::from_utf8_lossy(raw_line)
                .trim_end_matches('\r')
                .to_string();
            lines.push(line);
            start = end + 1;
        }
        if start > 0 {
            self.pending.drain(..start);
        }
        lines
    }
}

#[derive(Debug, Default)]
struct AbortTasksOnDrop(Vec<JoinHandle<()>>);

impl Drop for AbortTasksOnDrop {
    fn drop(&mut self) {
        for handle in self.0.drain(..) {
            handle.abort();
        }
    }
}

#[derive(Clone)]
pub struct NativeWakeMonitor {
    enabled: bool,
    state: Arc<Mutex<NativeInterlockState>>,
    tx: broadcast::Sender<()>,
    _tasks: Arc<AbortTasksOnDrop>,
}

impl NativeWakeMonitor {
    pub fn disabled() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAP);
        Self {
            enabled: false,
            state: Arc::new(Mutex::new(NativeInterlockState::from_config(
                &NativeInterlockConfig::default(),
            ))),
            tx,
            _tasks: Arc::new(AbortTasksOnDrop::default()),
        }
    }

    pub fn spawn(cfg: &NativeInterlockConfig) -> Self {
        if !cfg.enabled {
            return Self::disabled();
        }

        let state = Arc::new(Mutex::new(NativeInterlockState::from_config(cfg)));
        let (tx, _) = broadcast::channel(BROADCAST_CAP);
        let mut handles = Vec::new();

        let syslog_candidates = build_syslog_candidates(cfg.syslog_path.as_deref());
        let inst_candidates = cfg
            .instruction_log_path
            .as_ref()
            .map(|p| vec![p.clone()])
            .unwrap_or_default();

        if !syslog_candidates.is_empty() || !inst_candidates.is_empty() {
            let poll_interval = Duration::from_millis(cfg.poll_interval_ms.clamp(20, 1000));
            let state_clone = state.clone();
            let tx_clone = tx.clone();
            handles.push(tokio::spawn(run_log_tail_loop(
                syslog_candidates,
                inst_candidates,
                poll_interval,
                state_clone,
                tx_clone,
            )));
        }

        let event_cmd = cfg.event_command.trim().to_string();
        if !event_cmd.is_empty() {
            let state_clone = state.clone();
            let tx_clone = tx.clone();
            handles.push(tokio::spawn(run_event_command_loop(
                event_cmd,
                state_clone,
                tx_clone,
            )));
        }

        Self {
            enabled: true,
            state,
            tx,
            _tasks: Arc::new(AbortTasksOnDrop(handles)),
        }
    }

    pub fn is_blocked(&self) -> bool {
        if !self.enabled {
            return false;
        }
        let now = Instant::now();
        self.state
            .lock()
            .expect("native interlock mutex poisoned")
            .is_blocked(now)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.tx.subscribe()
    }

    pub fn notify_native_utterance(&self) {
        if !self.enabled {
            return;
        }
        let emit = self
            .state
            .lock()
            .expect("native interlock mutex poisoned")
            .trigger_external(Instant::now());
        if emit {
            let _ = self.tx.send(());
        }
    }
}

fn build_syslog_candidates(primary: Option<&Path>) -> Vec<PathBuf> {
    let Some(primary) = primary else {
        return Vec::new();
    };
    let mut out = vec![primary.to_path_buf()];
    if primary == Path::new("/tmp/log/messages") {
        out.push(PathBuf::from("/var/log/messages"));
    }
    out
}

/// Await the next native wake notification (returns on lagged messages too so wakes are never dropped).
pub async fn wait_native_wake(rx: &mut broadcast::Receiver<()>) {
    loop {
        match rx.recv().await {
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => return,
            Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
        }
    }
}

/// Drain any queued native wake notifications non-blockingly.
pub fn drain_native_rx(rx: &mut broadcast::Receiver<()>) {
    loop {
        match rx.try_recv() {
            Ok(()) | Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break;
            }
        }
    }
}

async fn run_log_tail_loop(
    syslog_candidates: Vec<PathBuf>,
    inst_candidates: Vec<PathBuf>,
    poll_interval: Duration,
    state: Arc<Mutex<NativeInterlockState>>,
    tx: broadcast::Sender<()>,
) {
    let mut syslog_tailer =
        (!syslog_candidates.is_empty()).then(|| LogTailer::new(syslog_candidates, true));
    let mut inst_tailer =
        (!inst_candidates.is_empty()).then(|| LogTailer::new(inst_candidates, true));

    loop {
        sleep(poll_interval).await;

        let syslog_lines = syslog_tailer
            .as_mut()
            .map(LogTailer::poll_new_lines)
            .unwrap_or_default();
        let inst_lines = inst_tailer
            .as_mut()
            .map(LogTailer::poll_new_lines)
            .unwrap_or_default();

        if syslog_lines.is_empty() && inst_lines.is_empty() {
            continue;
        }

        let now = Instant::now();
        let mut emit_wake = false;
        let mut wake_source = "";
        {
            let mut st = state.lock().expect("native interlock mutex poisoned");
            for line in &syslog_lines {
                if st.observe_event_line(line, now) {
                    emit_wake = true;
                    wake_source = "syslog";
                }
            }
            for line in &inst_lines {
                if st.observe_instruction_line(line, now) {
                    emit_wake = true;
                    if wake_source.is_empty() {
                        wake_source = "instruction_log";
                    }
                }
            }
        }
        if emit_wake {
            info!(source = wake_source, "检测到原生小爱唤醒，触发互锁让位");
            let _ = tx.send(());
        }
    }
}

struct EventProcessGroupGuard(Option<u32>);

impl EventProcessGroupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for EventProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.0.take() {
            let _ = std::process::Command::new("kill")
                .args(["-9", &format!("-{pgid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

async fn run_event_command_loop(
    event_cmd: String,
    state: Arc<Mutex<NativeInterlockState>>,
    tx: broadcast::Sender<()>,
) {
    loop {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(&event_cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        match cmd.spawn() {
            Ok(mut child) => {
                let mut guard = EventProcessGroupGuard(child.id());
                let Some(stdout) = child.stdout.take() else {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    guard.disarm();
                    sleep(Duration::from_secs(1)).await;
                    continue;
                };
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let now = Instant::now();
                            let emit = state
                                .lock()
                                .expect("native interlock mutex poisoned")
                                .observe_event_line(&line, now);
                            if emit {
                                info!(source = "event_command", "检测到原生小爱事件，触发互锁让位");
                                let _ = tx.send(());
                            }
                        }
                        Ok(None) => {
                            debug!("native_interlock event_command 流已结束，1s 后重连");
                            break;
                        }
                        Err(err) => {
                            warn!(error = %err, "读取 native_interlock event_command 输出失败");
                            break;
                        }
                    }
                }
                let _ = child.kill().await;
                let _ = child.wait().await;
                guard.disarm();
            }
            Err(err) => {
                warn!(cmd = %event_cmd, error = %err, "启动 native_interlock event_command 失败，1s 后重试");
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
}

// Re-export File check only if needed in tests
#[allow(dead_code)]
fn _touch_file_import(_: File) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tokio::time::timeout;

    fn temp_log_path(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "xiaoai-native-monitor-{tag}-{}-{stamp}.log",
            std::process::id()
        ))
    }

    #[test]
    fn syslog_state_machine_blocks_on_wake_expires_without_end_and_holds_cooldown() {
        let cfg = NativeInterlockConfig {
            max_active_s: 15,
            cooldown_s: 5,
            ..NativeInterlockConfig::default()
        };
        let mut st = NativeInterlockState::from_config(&cfg);
        let t0 = Instant::now();

        assert!(!st.is_blocked(t0));
        assert!(st.observe_event_line(
            "mipns-xiaomi[123]: send real_wakeup event",
            t0
        ));
        assert!(st.is_blocked(t0 + Duration::from_secs(1)));

        // Duplicate wake inside active window must not emit a second edge.
        assert!(!st.observe_event_line("wakeup.sh: asrstart", t0 + Duration::from_millis(200)));

        // Anti-deadlock cap: if asrend never arrives, active state must auto-expire at max_active_s.
        assert!(st.is_blocked(t0 + Duration::from_secs(14)));
        assert!(!st.is_blocked(t0 + Duration::from_secs(16)));

        // New wake followed by asrend -> cleared active + cooldown hold.
        let t1 = t0 + Duration::from_secs(20);
        assert!(st.observe_event_line("wakeup.sh: asrstart", t1));
        assert!(!st.observe_event_line("wakeup.sh: asrend", t1 + Duration::from_secs(2)));
        assert!(st.is_blocked(t1 + Duration::from_secs(6)));
        assert!(!st.is_blocked(t1 + Duration::from_secs(8)));
    }

    #[test]
    fn instruction_log_debounces_within_1s_of_syslog_and_acts_as_fallback() {
        let cfg = NativeInterlockConfig::default();
        let mut st = NativeInterlockState::from_config(&cfg);
        let t0 = Instant::now();

        // When syslog already emitted wake, instruction log within active window must not double-emit.
        assert!(st.observe_event_line("send real_wakeup event", t0));
        assert!(!st.observe_instruction_line(
            r#"{"header":{"namespace":"SpeechRecognizer","name":"Recognize"}}"#,
            t0 + Duration::from_millis(150)
        ));

        // After active/cooldown expires, instruction log alone acts as safety fallback.
        let t1 = t0 + Duration::from_secs(30);
        assert!(st.observe_instruction_line(
            r#"{"header":{"namespace":"SpeechRecognizer","name":"RecognizeResult"}}"#,
            t1
        ));
        assert!(st.is_blocked(t1 + Duration::from_secs(3)));
        assert!(!st.is_blocked(t1 + Duration::from_secs(6)));
    }

    #[test]
    fn log_tailer_skips_historic_lines_handles_partial_reads_and_rotation() {
        let path = temp_log_path("tailer");
        std::fs::write(&path, "old mipns-xiaomi: send real_wakeup event\n").unwrap();

        let mut tailer = LogTailer::new(vec![path.clone()], true);
        assert!(
            tailer.poll_new_lines().is_empty(),
            "启动前历史日志必须被跳过，否则开机就误触发让位"
        );

        // Append partial line, then finish it.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"partial line").unwrap();
        }
        assert!(tailer.poll_new_lines().is_empty());
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b" completed\nline two\n").unwrap();
        }
        assert_eq!(
            tailer.poll_new_lines(),
            vec!["partial line completed", "line two"]
        );

        // Simulate log truncation (e.g. copytruncate) -> tailer resets offset cleanly.
        std::fs::write(&path, "after truncate\n").unwrap();
        assert_eq!(tailer.poll_new_lines(), vec!["after truncate"]);

        // Simulate log rotation via unlink + create new inode.
        let rotated = temp_log_path("tailer-rot");
        std::fs::write(&rotated, "after rotation\n").unwrap();
        std::fs::rename(&rotated, &path).unwrap();
        assert_eq!(tailer.poll_new_lines(), vec!["after rotation"]);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn monitor_fires_from_appended_syslog_and_clears_after_asrend() {
        let syslog = temp_log_path("live-syslog");
        std::fs::write(&syslog, "historical send real_wakeup event\n").unwrap();

        let cfg = NativeInterlockConfig {
            enabled: true,
            event_command: String::new(),
            syslog_path: Some(syslog.clone()),
            instruction_log_path: None,
            poll_interval_ms: 20,
            max_active_s: 10,
            cooldown_s: 1,
            ..NativeInterlockConfig::default()
        };
        let monitor = NativeWakeMonitor::spawn(&cfg);
        let mut rx = monitor.subscribe();

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!monitor.is_blocked());

        {
            let mut f = OpenOptions::new().append(true).open(&syslog).unwrap();
            writeln!(f, "daemon: mipns-xiaomi[42]: send real_wakeup event").unwrap();
        }

        timeout(Duration::from_secs(2), wait_native_wake(&mut rx))
            .await
            .expect("syslog append should notify subscriber");
        assert!(monitor.is_blocked());

        let _ = std::fs::remove_file(&syslog);
    }

    #[tokio::test]
    async fn monitor_fires_from_event_command_stream() {
        let cfg = NativeInterlockConfig {
            enabled: true,
            event_command: "printf 'event send real_wakeup event\\n'; sleep 5".to_string(),
            syslog_path: None,
            instruction_log_path: None,
            ..NativeInterlockConfig::default()
        };
        let monitor = NativeWakeMonitor::spawn(&cfg);
        let mut rx = monitor.subscribe();

        timeout(Duration::from_secs(2), wait_native_wake(&mut rx))
            .await
            .expect("event_command stdout line should trigger wait_native_wake");
        assert!(monitor.is_blocked());
    }
}

//! Read-only non-blocking physical stop-key listener (`T-205`).
//!
//! 1:1 port of `xiaoai-doubao/xiaoai-plus/src/input/stop_key.cpp`:
//! - Disabled by default (`device == ""` or `key_code <= 0`)
//! - Opens `/dev/input/event*` with `O_RDONLY | O_NONBLOCK` and **never** calls `EVIOCGRAB`
//! - Ignores key release (`value == 0`), autorepeat (`value == 2`), duplicate downs,
//!   and enforces configurable `debounce_ms` (default 300 ms)
//! - Active dialogue sessions select on its signal; standby/native playback stays untouched

use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::config::StopKeyConfig;

const EV_KEY: u16 = 0x01;

/// Raw decoded subset of Linux `struct input_event`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedInputEvent {
    pub event_type: u16,
    pub code: u16,
    pub value: i32,
}

/// Decode one native Linux `struct input_event` byte slice (`16` bytes on 32-bit
/// targets such as LX06 ARMv7, `24` bytes on 64-bit Linux).
pub fn parse_input_event_bytes(bytes: &[u8]) -> Option<DecodedInputEvent> {
    let timeval_bytes = std::mem::size_of::<isize>() * 2;
    let total_bytes = timeval_bytes + 8;
    if bytes.len() < total_bytes {
        return None;
    }
    let tail = &bytes[timeval_bytes..total_bytes];
    Some(DecodedInputEvent {
        event_type: u16::from_ne_bytes([tail[0], tail[1]]),
        code: u16::from_ne_bytes([tail[2], tail[3]]),
        value: i32::from_ne_bytes([tail[4], tail[5], tail[6], tail[7]]),
    })
}

/// Stateful filter implementing `stop_key.cpp` hold + 300 ms debounce rules.
#[derive(Debug, Default)]
pub struct StopKeyFilter {
    held: bool,
    last_trigger: Option<Instant>,
}

impl StopKeyFilter {
    pub fn process_event(
        &mut self,
        ev: DecodedInputEvent,
        target_key_code: u16,
        debounce: Duration,
        now: Instant,
    ) -> bool {
        if ev.event_type != EV_KEY || ev.code != target_key_code {
            return false;
        }
        if ev.value == 0 {
            self.held = false;
            return false;
        }
        // Ignore autorepeat (2) and duplicate key-down (1 while already held).
        if ev.value != 1 || self.held {
            return false;
        }
        self.held = true;
        if let Some(prev) = self.last_trigger {
            if now.duration_since(prev) < debounce {
                return false;
            }
        }
        self.last_trigger = Some(now);
        true
    }
}

/// Background evdev listener emitting unit signals when the configured key is pressed.
pub struct StopKeyListener {
    #[cfg(target_os = "linux")]
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    #[cfg(target_os = "linux")]
    task: Option<tokio::task::JoinHandle<()>>,
    rx: mpsc::Receiver<()>,
}

impl StopKeyListener {
    /// Start listening when `cfg` has a non-empty device and positive `key_code`.
    /// When disabled (the default), returns a dormant listener that never fires.
    pub fn start(cfg: &StopKeyConfig) -> Self {
        let (_dummy_tx, rx) = mpsc::channel(4);
        let device = cfg.device.trim();
        if device.is_empty() || cfg.key_code <= 0 || cfg.key_code > u16::MAX as i32 {
            return Self {
                #[cfg(target_os = "linux")]
                stop_tx: None,
                #[cfg(target_os = "linux")]
                task: None,
                rx,
            };
        }

        #[cfg(target_os = "linux")]
        {
            return Self::start_linux(device, cfg.key_code as u16, cfg.debounce_ms);
        }

        #[cfg(not(target_os = "linux"))]
        {
            tracing::debug!(
                device,
                key_code = cfg.key_code,
                "stop_key listener is a no-op on non-Linux hosts"
            );
            Self { rx }
        }
    }

    #[cfg(target_os = "linux")]
    fn start_linux(device: &str, key_code: u16, debounce_ms: u64) -> Self {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        use tokio::io::unix::AsyncFd;

        const LINUX_O_NONBLOCK: i32 = 0o4000;

        let (tx, rx) = mpsc::channel(4);
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(LINUX_O_NONBLOCK)
            .open(device)
        {
            Ok(f) => f,
            Err(err) => {
                tracing::warn!(
                    device,
                    key_code,
                    error = %err,
                    "failed to open stop_key evdev device; physical stop key disabled"
                );
                return Self {
                    stop_tx: None,
                    task: None,
                    rx,
                };
            }
        };

        let async_fd = match AsyncFd::new(file) {
            Ok(fd) => fd,
            Err(err) => {
                tracing::warn!(
                    device,
                    error = %err,
                    "failed to register stop_key fd with tokio AsyncFd; physical stop key disabled"
                );
                return Self {
                    stop_tx: None,
                    task: None,
                    rx,
                };
            }
        };

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let debounce = Duration::from_millis(debounce_ms);
        let device_owned = device.to_string();
        let event_size = std::mem::size_of::<isize>() * 2 + 8;

        let task = tokio::spawn(async move {
            let mut filter = StopKeyFilter::default();
            let mut buf = [0u8; 32];
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    ready_res = async_fd.readable() => {
                        let mut guard = match ready_res {
                            Ok(g) => g,
                            Err(err) => {
                                tracing::warn!(device = %device_owned, error = %err, "input listener stopped; key exit unavailable");
                                break;
                            }
                        };
                        match guard.try_io(|inner| inner.get_ref().read(&mut buf[..event_size])) {
                            Ok(Ok(n)) if n == event_size => {
                                if let Some(ev) = parse_input_event_bytes(&buf[..n]) {
                                    if filter.process_event(ev, key_code, debounce, Instant::now()) {
                                        let _ = tx.try_send(());
                                    }
                                }
                            }
                            Ok(Ok(0)) => {
                                tracing::warn!(device = %device_owned, "input listener EOF; key exit unavailable");
                                break;
                            }
                            Ok(Ok(_)) => continue,
                            Ok(Err(err)) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                            Ok(Err(err)) => {
                                tracing::warn!(device = %device_owned, error = %err, "input listener stopped; key exit unavailable");
                                break;
                            }
                            Err(_would_block) => continue,
                        }
                    }
                }
            }
        });

        tracing::info!(
            device,
            key_code,
            debounce_ms,
            "stop_key physical read-only listener active"
        );
        Self {
            stop_tx: Some(stop_tx),
            task: Some(task),
            rx,
        }
    }

    /// Receive the next debounced key press event. Pending forever when disabled.
    pub async fn recv(&mut self) -> Option<()> {
        self.rx.recv().await
    }

    /// Drop any queued press events that arrived while idle (standby must ignore them).
    pub fn drain_pending(&mut self) {
        while self.rx.try_recv().is_ok() {}
    }

    pub fn receiver_mut(&mut self) -> &mut mpsc::Receiver<()> {
        &mut self.rx
    }
}

impl Drop for StopKeyListener {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if let Some(tx) = self.stop_tx.take() {
                let _ = tx.send(());
            }
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_test_input_event(event_type: u16, code: u16, value: i32) -> Vec<u8> {
        let timeval_bytes = std::mem::size_of::<isize>() * 2;
        let mut buf = vec![0u8; timeval_bytes + 8];
        buf[timeval_bytes..timeval_bytes + 2].copy_from_slice(&event_type.to_ne_bytes());
        buf[timeval_bytes + 2..timeval_bytes + 4].copy_from_slice(&code.to_ne_bytes());
        buf[timeval_bytes + 4..timeval_bytes + 8].copy_from_slice(&value.to_ne_bytes());
        buf
    }

    #[test]
    fn disabled_config_creates_dormant_listener() {
        let cfg = StopKeyConfig::default();
        let mut listener = StopKeyListener::start(&cfg);
        listener.drain_pending();
        assert_eq!(cfg.key_code, -1);
    }

    #[test]
    fn parses_synthetic_input_event_bytes_and_enforces_hold_and_debounce() {
        let press = parse_input_event_bytes(&encode_test_input_event(EV_KEY, 164, 1)).unwrap();
        let repeat = parse_input_event_bytes(&encode_test_input_event(EV_KEY, 164, 2)).unwrap();
        let release = parse_input_event_bytes(&encode_test_input_event(EV_KEY, 164, 0)).unwrap();
        let other_key = parse_input_event_bytes(&encode_test_input_event(EV_KEY, 115, 1)).unwrap();
        let non_key = parse_input_event_bytes(&encode_test_input_event(0x00, 164, 1)).unwrap();

        let mut filter = StopKeyFilter::default();
        let t0 = Instant::now();
        let debounce = Duration::from_millis(300);

        // Wrong event type / wrong key code are ignored.
        assert!(!filter.process_event(non_key, 164, debounce, t0));
        assert!(!filter.process_event(other_key, 164, debounce, t0));

        // First press triggers.
        assert!(filter.process_event(press, 164, debounce, t0));

        // Autorepeat and duplicate press while held are ignored.
        assert!(!filter.process_event(repeat, 164, debounce, t0 + Duration::from_millis(400)));
        assert!(!filter.process_event(press, 164, debounce, t0 + Duration::from_millis(400)));

        // Release clears held state, but re-press within 300 ms debounce window is ignored.
        assert!(!filter.process_event(release, 164, debounce, t0 + Duration::from_millis(50)));
        assert!(!filter.process_event(press, 164, debounce, t0 + Duration::from_millis(200)));

        // Release again and press after >= 300 ms triggers cleanly.
        assert!(!filter.process_event(release, 164, debounce, t0 + Duration::from_millis(220)));
        assert!(filter.process_event(press, 164, debounce, t0 + Duration::from_millis(300)));
    }
}

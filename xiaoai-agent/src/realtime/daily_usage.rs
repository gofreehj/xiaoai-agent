//! Local daily token budget guard (`T-204`).
//!
//! Tracks daily Realtime token usage in an atomic JSON file under
//! `config.realtime.daily_usage.state_dir` (default `/tmp/xiaoai-agent/usage`
//! on tmpfs to avoid LX06 NAND flash wear). Automatically resets across local
//! calendar dates and degrades gracefully if the state file is corrupted.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Local;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::DailyUsageConfig;

const USAGE_FILE_NAME: &str = "daily_usage.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct UsageRecord {
    date: String,
    tokens: u64,
}

/// Thread-safe local daily token counter and quota guard.
pub struct DailyUsageGuard {
    state_dir: PathBuf,
    daily_token_limit: u64,
    lock: Mutex<()>,
}

impl DailyUsageGuard {
    pub fn from_config(cfg: &DailyUsageConfig) -> Self {
        Self::new(cfg.state_dir.clone(), cfg.daily_token_limit)
    }

    pub fn new(state_dir: impl Into<PathBuf>, daily_token_limit: u64) -> Self {
        Self {
            state_dir: state_dir.into(),
            daily_token_limit,
            lock: Mutex::new(()),
        }
    }

    pub fn daily_token_limit(&self) -> u64 {
        self.daily_token_limit
    }

    /// Return `true` when `daily_token_limit > 0` and today's accumulated usage
    /// has reached or exceeded the cap.
    pub fn is_limit_exceeded(&self) -> bool {
        let today = current_date_string();
        self.is_limit_exceeded_for_date(&today)
    }

    /// Read today's accumulated tokens (returns `0` on cross-day or corrupted JSON).
    pub fn read_today(&self) -> u64 {
        let today = current_date_string();
        self.read_for_date(&today)
    }

    /// Accumulate `tokens` for today and return the updated total.
    pub fn add_today(&self, tokens: u64) -> u64 {
        let today = current_date_string();
        self.add_for_date(&today, tokens)
    }

    pub fn is_limit_exceeded_for_date(&self, date: &str) -> bool {
        self.daily_token_limit > 0 && self.read_for_date(date) >= self.daily_token_limit
    }

    pub fn read_for_date(&self, date: &str) -> u64 {
        let _guard = self.lock.lock().unwrap();
        self.read_locked(date)
    }

    pub fn add_for_date(&self, date: &str, tokens: u64) -> u64 {
        if tokens == 0 {
            return self.read_for_date(date);
        }
        let _guard = self.lock.lock().unwrap();
        let current = self.read_locked(date);
        let updated = current.saturating_add(tokens);
        self.write_locked(date, updated);
        updated
    }

    fn file_path(&self) -> PathBuf {
        self.state_dir.join(USAGE_FILE_NAME)
    }

    fn read_locked(&self, date: &str) -> u64 {
        if self.state_dir.as_os_str().is_empty() {
            return 0;
        }
        let path = self.file_path();
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(err) if err.kind() == ErrorKind::NotFound => return 0,
            Err(err) => {
                warn!(path = %path.display(), error = %err, "failed to read daily usage file; defaulting to 0");
                return 0;
            }
        };
        let record: UsageRecord = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "corrupted daily usage JSON; resetting counter cleanly"
                );
                return 0;
            }
        };
        if record.date == date {
            record.tokens
        } else {
            0
        }
    }

    fn write_locked(&self, date: &str, tokens: u64) {
        if self.state_dir.as_os_str().is_empty() {
            return;
        }
        if let Err(err) = ensure_private_dir(&self.state_dir) {
            warn!(dir = %self.state_dir.display(), error = %err, "failed to prepare daily usage directory");
            return;
        }
        let record = UsageRecord {
            date: date.to_string(),
            tokens,
        };
        let Ok(json) = serde_json::to_string(&record) else {
            return;
        };
        let path = self.file_path();
        let tmp_path = self.state_dir.join(format!(".{USAGE_FILE_NAME}.tmp"));
        if let Err(err) = write_private_file(&tmp_path, json.as_bytes()) {
            warn!(path = %tmp_path.display(), error = %err, "failed to write daily usage temp file");
            return;
        }
        if let Err(err) = fs::rename(&tmp_path, &path) {
            warn!(path = %path.display(), error = %err, "failed to commit daily usage file");
            let _ = fs::remove_file(&tmp_path);
        }
    }
}

fn current_date_string() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_usage_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "xiaoai_daily_usage_{name}_{}",
            std::process::id()
        ))
    }

    #[test]
    fn accumulates_resets_across_dates_and_enforces_limit() {
        let dir = temp_usage_dir("limit_reset");
        let _ = fs::remove_dir_all(&dir);

        let guard = DailyUsageGuard::new(&dir, 1000);
        assert_eq!(guard.read_for_date("2026-11-28"), 0);
        assert!(!guard.is_limit_exceeded_for_date("2026-11-28"));

        assert_eq!(guard.add_for_date("2026-11-28", 400), 400);
        assert!(!guard.is_limit_exceeded_for_date("2026-11-28"));

        assert_eq!(guard.add_for_date("2026-11-28", 600), 1000);
        assert!(guard.is_limit_exceeded_for_date("2026-11-28"));

        // New calendar day resets usage automatically.
        assert_eq!(guard.read_for_date("2026-11-29"), 0);
        assert!(!guard.is_limit_exceeded_for_date("2026-11-29"));
        assert_eq!(guard.add_for_date("2026-11-29", 50), 50);

        // Limit = 0 disables blocking even when tokens > 0.
        let unlimited = DailyUsageGuard::new(&dir, 0);
        assert!(!unlimited.is_limit_exceeded_for_date("2026-11-29"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupted_json_is_tolerated_and_recovers_cleanly() {
        let dir = temp_usage_dir("corrupt");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(USAGE_FILE_NAME), b"{not-valid-json!!!").unwrap();

        let guard = DailyUsageGuard::new(&dir, 500);
        assert_eq!(guard.read_for_date("2026-11-28"), 0);
        assert!(!guard.is_limit_exceeded_for_date("2026-11-28"));
        assert_eq!(guard.add_for_date("2026-11-28", 120), 120);
        assert_eq!(guard.read_for_date("2026-11-28"), 120);

        let _ = fs::remove_dir_all(&dir);
    }
}

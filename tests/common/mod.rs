//! Fixtures shared by the end-to-end tests that need a throwaway `CODEX_HOME`.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct TempDir(pub PathBuf);
impl TempDir {
    pub fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("quotaline-it-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Hinnant, howardhinnant.github.io/date_algorithms.html#civil_from_days
pub fn rfc3339(epoch: i64) -> String {
    let z = epoch.div_euclid(86_400) + 719_468;
    let secs = epoch.rem_euclid(86_400);
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

pub fn event(t: i64, pct: f64, reset: i64, tin: i64, tout: i64) -> String {
    serde_json::json!({
        "timestamp": rfc3339(t),
        "type": "event_msg",
        "payload": {
            "type": "token_count",
            "info": {"total_token_usage": {"input_tokens": tin, "output_tokens": tout}},
            "rate_limits": {
                "primary": {"used_percent": pct, "window_minutes": 10080, "resets_at": reset},
                "secondary": null
            }
        }
    })
    .to_string()
}

pub fn write_rollout(home: &Path, lines: &[String]) {
    let dir = home.join("sessions/2026/10/26");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("rollout-a.jsonl"), lines.join("\n") + "\n").unwrap();
}

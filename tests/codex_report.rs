//! `quotaline report --codex` end to end, against a throwaway `CODEX_HOME`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
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

fn report(codex_home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_quotaline"))
        .args(["report", "--codex"])
        .env("CODEX_HOME", codex_home)
        .env("CTT_STATE_DIR", codex_home)
        .output()
        .unwrap()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Hinnant, howardhinnant.github.io/date_algorithms.html#civil_from_days
fn rfc3339(epoch: i64) -> String {
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

fn event(t: i64, pct: f64, reset: i64, tin: i64, tout: i64) -> String {
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

fn write_rollout(home: &Path, lines: &[String]) {
    let dir = home.join("sessions/2026/10/26");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("rollout-a.jsonl"), lines.join("\n") + "\n").unwrap();
}

#[test]
fn reports_rate_and_token_burn() {
    let d = TempDir::new("live");
    let now = now();
    let reset = now + 5 * 86_400;
    write_rollout(
        &d.0,
        &[
            event(now - 3_660, 1.0, reset, 100_000, 1_000),
            event(now - 60, 3.0, reset, 300_000, 4_000),
        ],
    );
    let out = report(&d.0);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("+2.0%/hr"), "{stdout}");
    assert!(
        stdout.contains("in 300k · out 4k over the last 2h   ≈152k/hr"),
        "{stdout}"
    );
}

#[test]
fn a_burst_days_ago_reads_idle() {
    let d = TempDir::new("stale");
    let then = now() - 3 * 86_400;
    let reset = then + 4 * 86_400;
    write_rollout(
        &d.0,
        &[
            event(then - 3_600, 0.0, reset, 0, 0),
            event(then - 1_800, 9.0, reset, 1_000, 10),
            event(then, 18.0, reset, 2_000, 20),
        ],
    );
    let out = report(&d.0);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("~idle (no measurable burn)"), "{stdout}");
    assert!(!stdout.contains("hits cap first"), "{stdout}");
}

#[test]
fn an_explicit_codex_home_without_sessions_is_an_error() {
    let d = TempDir::new("empty");
    let out = report(&d.0);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("CODEX_HOME has no sessions directory"));
}

//! `quotaline report --codex` end to end, against a throwaway `CODEX_HOME`.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::{event, now, write_rollout, TempDir};

fn report(codex_home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_quotaline"))
        .args(["report", "--codex"])
        .env("CODEX_HOME", codex_home)
        .env("CTT_STATE_DIR", codex_home)
        .output()
        .unwrap()
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
    assert!(
        stdout.contains("(2 events over 1h, last 1m ago)"),
        "{stdout}"
    );
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

#[test]
fn codex_home_flag_overrides_the_environment() {
    let d = TempDir::new("flag");
    let now = now();
    write_rollout(
        &d.0.join("work"),
        &[event(now - 60, 4.0, now + 5 * 86_400, 10, 1)],
    );
    let out = Command::new(env!("CARGO_BIN_EXE_quotaline"))
        .args(["report", "--codex", "--codex-home"])
        .arg(d.0.join("work"))
        .env("CODEX_HOME", d.0.join("elsewhere"))
        .env("CTT_STATE_DIR", &d.0)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("  4%"), "{stdout}");
}

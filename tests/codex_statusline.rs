//! The status line's `cx` line, end to end, against a throwaway Codex home.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use common::{event, now, write_rollout, TempDir};

/// The rendered status line for `payload` at terminal width `columns`, run with `args`.
fn run(dir: &TempDir, args: &[&str], payload: &str, columns: u32) -> String {
    let input = dir.0.join("payload.json");
    fs::write(&input, payload).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_quotaline"))
        .args(args)
        .env("CODEX_HOME", &dir.0)
        .env("CTT_STATE_DIR", &dir.0)
        .env("COLUMNS", columns.to_string())
        .stdin(Stdio::from(fs::File::open(&input).unwrap()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out.status);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// [`run`] with the status line pointed at `dir` as the Codex home.
fn statusline(dir: &TempDir, payload: &str, columns: u32) -> String {
    run(dir, &["--codex-home", path(&dir.0)], payload, columns)
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn subscription(five_reset: i64, week_reset: i64) -> String {
    format!(
        r#"{{"session_id":"s1","rate_limits":{{"five_hour":{{"used_percentage":18,"resets_at":{five_reset}}},"seven_day":{{"used_percentage":30,"resets_at":{week_reset}}}}}}}"#
    )
}

fn cx_line(out: &str) -> Option<&str> {
    out.lines().find(|l| l.contains("cx\u{1b}[0m"))
}

fn strip(line: &str) -> String {
    let mut out = String::new();
    let mut in_escape = false;
    for c in line.chars() {
        match (in_escape, c) {
            (false, '\u{1b}') => in_escape = true,
            (true, 'm') => in_escape = false,
            (true, _) => {}
            (false, c) => out.push(c),
        }
    }
    out
}

#[test]
fn a_current_codex_window_adds_a_cx_line_under_the_weekly_bar() {
    let d = TempDir::new("cx-sub");
    let now = now();
    write_rollout(&d.0, &[event(now - 60, 7.0, now + 3 * 86_400, 10, 1)]);
    let out = statusline(&d, &subscription(now + 3600, now + 5 * 86_400), 80);
    let cx = cx_line(&out).unwrap_or_else(|| panic!("{out}"));
    assert!(cx.contains(" 7%"), "{out}");
    assert_eq!(out.lines().last(), Some(cx), "{out}");
}

#[test]
fn the_cx_bar_is_off_without_codex_home() {
    let d = TempDir::new("cx-off");
    let now = now();
    write_rollout(&d.0, &[event(now - 60, 7.0, now + 3 * 86_400, 10, 1)]);
    let out = run(&d, &[], &subscription(now + 3600, now + 5 * 86_400), 80);
    assert_eq!(cx_line(&out), None, "{out}");
}

#[test]
fn an_empty_codex_home_is_no_codex_home() {
    let d = TempDir::new("cx-empty-flag");
    let now = now();
    write_rollout(&d.0, &[event(now - 60, 7.0, now + 3 * 86_400, 10, 1)]);
    let input = d.0.join("payload.json");
    fs::write(&input, subscription(now + 3600, now + 5 * 86_400)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_quotaline"))
        .args(["--codex-home", ""])
        .current_dir(&d.0)
        .env("CTT_STATE_DIR", &d.0)
        .stdin(Stdio::from(fs::File::open(&input).unwrap()))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert_eq!(cx_line(&out), None, "{out}");
}

#[test]
fn codex_home_may_be_given_relative_to_home() {
    let d = TempDir::new("cx-tilde");
    let now = now();
    write_rollout(
        &d.0.join("codex"),
        &[event(now - 60, 7.0, now + 3 * 86_400, 10, 1)],
    );
    let input = d.0.join("payload.json");
    fs::write(&input, subscription(now + 3600, now + 5 * 86_400)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_quotaline"))
        .args(["--codex-home", "~/codex"])
        .env("HOME", &d.0)
        .env("CTT_STATE_DIR", &d.0)
        .stdin(Stdio::from(fs::File::open(&input).unwrap()))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(cx_line(&out).is_some_and(|l| l.contains(" 7%")), "{out}");
}

#[test]
fn a_cx_reset_longer_than_the_claude_ones_still_fits_the_terminal() {
    let d = TempDir::new("cx-fit");
    let now = now();
    // cx's reset text is the widest, so it alone sets the bar width
    write_rollout(
        &d.0,
        &[event(
            now - 60,
            7.0,
            now + 6 * 86_400 + 22 * 3600 + 1800,
            10,
            1,
        )],
    );
    let out = statusline(&d, &subscription(now + 3500, now + 3500), 60);
    assert!(cx_line(&out).is_some(), "{out}");
    for line in out.lines() {
        // SAFE_MARGIN, src/statusline.rs
        assert!(strip(line).chars().count() <= 60 - 8, "{line:?}\n{out}");
    }
}

#[test]
fn the_cx_bar_aligns_with_the_claude_bars() {
    let d = TempDir::new("cx-align");
    let now = now();
    write_rollout(&d.0, &[event(now - 60, 7.0, now + 3500, 10, 1)]);
    let out = statusline(&d, &subscription(now + 3600, now + 5 * 86_400), 80);
    let bars: Vec<String> = out
        .lines()
        .map(strip)
        .skip_while(|l| !l.starts_with("5h"))
        .collect();
    assert_eq!(bars.len(), 3, "{out}");
    // a shorter cx reset text than wk's, so only a shared width lines the bars up
    assert!(bars[2].chars().count() < bars[1].chars().count(), "{out}");
    let pct_col = |l: &String| l.chars().position(|c| c == '%');
    assert!(
        bars.iter().all(|l| pct_col(l) == pct_col(&bars[0])),
        "{out}"
    );
}

#[test]
fn a_cx_line_follows_a_render_with_no_claude_windows() {
    let d = TempDir::new("cx-none");
    let now = now();
    write_rollout(&d.0, &[event(now - 60, 7.0, now + 3 * 86_400, 10, 1)]);
    let out = statusline(&d, "{}", 80);
    assert!(out.starts_with("\u{1b}[90mlimits n/a"), "{out}");
    assert!(cx_line(&out).is_some_and(|l| l.contains(" 7%")), "{out}");
}

#[test]
fn the_api_key_render_gets_a_cx_line() {
    let d = TempDir::new("cx-api");
    let now = now();
    write_rollout(&d.0, &[event(now - 60, 7.0, now + 3 * 86_400, 10, 1)]);
    let out = statusline(
        &d,
        r#"{"session_id":"s1","cost":{"total_cost_usd":1.25}}"#,
        80,
    );
    assert!(out.contains("day"), "{out}");
    assert!(cx_line(&out).is_some_and(|l| l.contains(" 7%")), "{out}");
}

#[test]
fn a_standalone_cx_line_fits_a_narrow_terminal() {
    let d = TempDir::new("cx-narrow");
    let now = now();
    write_rollout(
        &d.0,
        &[event(
            now - 60,
            7.0,
            now + 6 * 86_400 + 22 * 3600 + 1800,
            10,
            1,
        )],
    );
    let out = statusline(&d, "{}", 60);
    let cx = cx_line(&out).unwrap_or_else(|| panic!("{out}"));
    // SAFE_MARGIN, src/statusline.rs
    assert!(strip(cx).chars().count() <= 60 - 8, "{cx:?}");
}

#[test]
fn no_cx_line_once_the_window_has_reset() {
    let d = TempDir::new("cx-reset");
    let now = now();
    write_rollout(&d.0, &[event(now - 8 * 86_400, 7.0, now - 60, 10, 1)]);
    let out = statusline(&d, &subscription(now + 3600, now + 5 * 86_400), 80);
    assert_eq!(cx_line(&out), None);
}

#[test]
fn no_cx_line_without_codex_sessions() {
    let d = TempDir::new("cx-empty");
    let now = now();
    let out = statusline(&d, &subscription(now + 3600, now + 5 * 86_400), 80);
    assert_eq!(cx_line(&out), None);
}

//! OpenAI Codex plan usage for `quotaline report --codex`, rebuilt on every run from the
//! `token_count` events in `$CODEX_HOME/sessions/**/rollout-*.jsonl`. Why logs rather than
//! stdin: README, `quotaline report --codex`.

use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::burn::Analysis;
use crate::history::Sample;
use crate::json::{f64_at, nested, str_at};

/// The weekly window, identified by duration: Codex names its windows `primary` and
/// `secondary` rather than by length.
const WEEK_MINUTES: f64 = 10_080.0;
pub const WEEK_SECS: f64 = WEEK_MINUTES * 60.0;

/// `resets_at` drifts by seconds between events of one window, while successive windows
/// start hours apart. Values this close are the same window.
const RESET_JITTER_SECS: f64 = 600.0;

/// `$CODEX_HOME` if set and non-empty, else `~/.codex`.
pub fn codex_home() -> Option<PathBuf> {
    resolve_home(std::env::var_os("CODEX_HOME"), crate::history::home_dir())
}

fn resolve_home(env: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match env {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => home.map(|h| h.join(".codex")),
    }
}

/// Every `token_count` event in rollouts modified at or after `since`, as samples ordered by
/// time: `d7`/`d7r` from the weekly window, `tin`/`tout` from the thread's running totals,
/// `sid` naming the rollout file. Unreadable files, lines and file tails are skipped.
pub fn load(sessions: &Path, since: f64) -> Vec<Sample> {
    let mut files = Vec::new();
    collect_rollouts(sessions, since, &mut files);
    let mut out = Vec::new();
    for path in files {
        read_rollout(&path, &mut out);
    }
    out.sort_by_key(|s| s.t);
    canonicalise_resets(&mut out);
    out
}

fn collect_rollouts(dir: &Path, since: f64, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            collect_rollouts(&path, since, out);
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
            continue;
        }
        // follows a symlink to its target; opening a FIFO would block forever
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        // appends bump mtime, so an older file holds no event at or after `since`
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64());
        if mtime.is_some_and(|m| m >= since) {
            out.push(path);
        }
    }
}

fn read_rollout(path: &Path, out: &mut Vec<Sample>) {
    let Ok(file) = fs::File::open(path) else {
        return;
    };
    let sid = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // most of a rollout is transcript; skip it before paying for a parse
        let Ok(line) = std::str::from_utf8(&buf) else {
            continue;
        };
        if !line.contains("\"token_count\"") {
            continue;
        }
        if let Some(mut s) = parse_event(line) {
            s.sid = sid.clone();
            out.push(s);
        }
    }
}

fn parse_event(line: &str) -> Option<Sample> {
    let v: Value = serde_json::from_str(line).ok()?;
    if str_at(&v, &["payload", "type"])? != "token_count" {
        return None;
    }
    let t = parse_utc(str_at(&v, &["timestamp"])?)?;
    let mut s = Sample {
        t: t as i64,
        ..Default::default()
    };
    if let Some(week) = nested(&v, &["payload", "rate_limits"]).and_then(weekly_window) {
        // a reading without its reset can't be placed in a window
        if let Some(reset) = f64_at(week, &["resets_at"]) {
            s.d7 = f64_at(week, &["used_percent"]);
            s.d7r = Some(reset);
        }
    }
    if let Some(usage) = nested(&v, &["payload", "info", "total_token_usage"]) {
        s.tin = f64_at(usage, &["input_tokens"]).map(|n| n as i64);
        s.tout = f64_at(usage, &["output_tokens"]).map(|n| n as i64);
    }
    Some(s)
}

fn weekly_window(limits: &Value) -> Option<&Value> {
    ["primary", "secondary"]
        .iter()
        .filter_map(|k| nested(limits, &[k]))
        .find(|w| f64_at(w, &["window_minutes"]) == Some(WEEK_MINUTES))
}

/// Snap each `d7r` to the first value seen for its window; see
/// `jittered_resets_stay_one_analysis_segment`. Expects `samples` sorted by time.
fn canonicalise_resets(samples: &mut [Sample]) {
    let mut seen: Vec<f64> = Vec::new();
    for s in samples.iter_mut() {
        let Some(r) = s.d7r else { continue };
        match seen.iter().find(|c| (r - **c).abs() <= RESET_JITTER_SECS) {
            Some(c) => s.d7r = Some(*c),
            None => seen.push(r),
        }
    }
}

/// `a.rate`, or `None` when fewer than two of its window's readings fall in the trailing
/// `window_sec`, so a burst days ago never reads as current burn; see `stale_burst_is_idle`.
pub fn recent_rate(samples: &[Sample], a: &Analysis, window_sec: f64, now: f64) -> Option<f64> {
    let start = now - window_sec;
    let recent = samples
        .iter()
        .filter(|s| s.d7.is_some() && s.d7r == a.reset && s.t as f64 >= start)
        .count();
    if recent >= 2 {
        a.rate
    } else {
        None
    }
}

/// Tokens burned across all threads over the trailing `window_sec`.
pub struct TokenBurn {
    pub tin: f64,
    pub tout: f64,
}

/// Per thread, its last running total in the window minus its last total before it. A
/// thread first seen inside the window counts from zero, since every rollout's counter
/// starts there. `None` when no thread recorded tokens in the window.
pub fn token_burn(samples: &[Sample], window_sec: f64, now: f64) -> Option<TokenBurn> {
    use std::collections::HashMap;
    type Totals = (i64, i64); // (tin, tout)
    let start = now - window_sec;
    // sid → (last total before the window, last total inside it)
    let mut by_sid: HashMap<&str, (Totals, Option<Totals>)> = HashMap::new();
    for s in samples {
        let (Some(sid), Some(tin), Some(tout)) = (s.sid.as_deref(), s.tin, s.tout) else {
            continue;
        };
        let t = s.t as f64;
        if t > now {
            continue;
        }
        let slot = by_sid.entry(sid).or_insert(((0, 0), None));
        if t < start {
            slot.0 = (tin, tout);
        } else {
            slot.1 = Some((tin, tout));
        }
    }
    let mut burn = TokenBurn {
        tin: 0.0,
        tout: 0.0,
    };
    let mut any = false;
    for ((base_in, base_out), last) in by_sid.values() {
        let Some((tin, tout)) = last else { continue };
        any = true;
        burn.tin += (tin - base_in).max(0) as f64;
        burn.tout += (tout - base_out).max(0) as f64;
    }
    any.then_some(burn)
}

/// An RFC 3339 timestamp (`2026-10-26T14:11:11.401Z`, or with a `±HH:MM` offset) as Unix
/// seconds.
fn parse_utc(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if b[16] != b':' || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut rest = &s[19..];
    let mut frac = 0.0;
    if let Some(r) = rest.strip_prefix('.') {
        let digits = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
        frac = format!("0.{}", &r[..digits]).parse().ok()?;
        rest = &r[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let oh = rest.get(1..3)?.parse::<i64>().ok()?;
            let om = rest.get(4..6)?.parse::<i64>().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset;
    Some(secs as f64 + frac)
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
/// Hinnant, howardhinnant.github.io/date_algorithms.html#days_from_civil
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burn::{analyze, Win};
    use std::fs::File;
    use std::io::Write;
    use std::time::{Duration, SystemTime};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir()
                .join(format!("quotaline-codex-test-{}-{tag}", std::process::id()));
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

    fn ts(epoch: i64) -> String {
        let days = epoch.div_euclid(86_400);
        let secs = epoch.rem_euclid(86_400);
        // inverse of days_from_civil, for fixtures only
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.123Z",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60
        )
    }

    fn event(t: i64, pct: f64, reset: f64, tin: i64, tout: i64) -> String {
        serde_json::json!({
            "timestamp": ts(t),
            "ordinal": 1,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {"total_token_usage": {
                    "input_tokens": tin, "cached_input_tokens": 0, "output_tokens": tout
                }},
                "rate_limits": {
                    "limit_id": "codex",
                    "primary": {"used_percent": pct, "window_minutes": 10080, "resets_at": reset},
                    "secondary": null,
                    "plan_type": "pro"
                }
            }
        })
        .to_string()
    }

    fn write(dir: &Path, rel: &str, lines: &[String]) -> PathBuf {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path
    }

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_utc("2026-10-26T14:11:11.5Z"), Some(1_793_023_871.5));
        assert_eq!(
            parse_utc("2026-10-26T15:11:11+01:00"),
            Some(1_793_023_871.0)
        );
        assert_eq!(
            parse_utc("2026-10-26T13:11:11-01:00"),
            Some(1_793_023_871.0)
        );
        assert_eq!(parse_utc("2000-02-29T00:00:00Z"), Some(951_782_400.0));
        assert_eq!(parse_utc("2026-10-26 14:11:11Z"), None);
        assert_eq!(parse_utc("2026-13-26T14:11:11Z"), None);
        assert_eq!(parse_utc("2026-10-32T14:11:11Z"), None);
        assert_eq!(parse_utc("2026-10-26T14:11:11"), None);
    }

    #[test]
    fn fixture_timestamps_round_trip() {
        for t in [0, 951_782_400, 1_793_023_871, 4_102_444_799] {
            assert_eq!(parse_utc(&ts(t)), Some(t as f64 + 0.123));
        }
    }

    #[test]
    fn codex_home_prefers_env_then_dot_codex() {
        let home = Some(PathBuf::from("/h"));
        assert_eq!(
            resolve_home(Some("/w".into()), home.clone()),
            Some(PathBuf::from("/w"))
        );
        assert_eq!(
            resolve_home(Some("".into()), home.clone()),
            Some(PathBuf::from("/h/.codex"))
        );
        assert_eq!(resolve_home(None, home), Some(PathBuf::from("/h/.codex")));
        assert_eq!(resolve_home(None, None), None);
    }

    #[test]
    fn load_keeps_token_count_events_only() {
        let d = TempDir::new("load");
        // passes the substring prefilter, so only the payload type check can reject it
        let decoy = serde_json::json!({
            "timestamp": ts(1500), "type": "response_item",
            "payload": {"type": "function_call", "name": "token_count"}
        });
        let rate_only = serde_json::json!({
            "timestamp": ts(1600), "type": "event_msg",
            "payload": {"type": "token_count", "info": null, "rate_limits": {
                "primary": {"used_percent": 4.0, "window_minutes": 300, "resets_at": 99.0},
                "secondary": {"used_percent": 3.0, "window_minutes": 10080, "resets_at": 9e5}
            }}
        });
        let no_reset = serde_json::json!({
            "timestamp": ts(1700), "type": "event_msg",
            "payload": {"type": "token_count", "info": null, "rate_limits": {
                "primary": {"used_percent": 7.0, "window_minutes": 10080}
            }}
        });
        write(
            &d.0,
            "2026/10/26/rollout-a.jsonl",
            &[
                event(1000, 1.0, 9e5, 10, 1),
                decoy.to_string(),
                rate_only.to_string(),
                no_reset.to_string(),
                "{\"timestamp\":\"2026-10-26T1".into(), // torn write
            ],
        );
        write(
            &d.0,
            "2026/10/26/rollout-c.json",
            &[event(1300, 9.0, 9e5, 1, 1)],
        );
        write(
            &d.0,
            "2026/10/25/rollout-b.jsonl",
            &[event(1200, 2.0, 9e5, 30, 3)],
        );
        write(
            &d.0,
            "2026/10/25/notes.jsonl",
            &[event(1300, 9.0, 9e5, 1, 1)],
        );

        let s = load(&d.0, 0.0);
        let got: Vec<_> = s
            .iter()
            .map(|s| (s.t, s.d7, s.tin, s.sid.as_deref().unwrap()))
            .collect();
        assert_eq!(
            got,
            vec![
                (1000, Some(1.0), Some(10), "rollout-a.jsonl"),
                (1200, Some(2.0), Some(30), "rollout-b.jsonl"),
                (1600, Some(3.0), None, "rollout-a.jsonl"),
                (1700, None, None, "rollout-a.jsonl"),
            ]
        );
        assert!(s.iter().all(|s| s.h5.is_none()));
    }

    #[test]
    fn load_skips_files_not_modified_since_cutoff() {
        let d = TempDir::new("mtime");
        let old = write(&d.0, "rollout-old.jsonl", &[event(1000, 1.0, 9e5, 10, 1)]);
        write(&d.0, "rollout-new.jsonl", &[event(1000, 2.0, 9e5, 10, 1)]);
        let cutoff = SystemTime::now() - Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(cutoff - Duration::from_secs(60))
            .unwrap();
        let since = cutoff.duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
        let s = load(&d.0, since);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].d7, Some(2.0));
    }

    #[cfg(unix)]
    #[test]
    fn load_follows_symlinked_rollouts() {
        let d = TempDir::new("symlink");
        let real = write(
            &d.0,
            "elsewhere/real.jsonl",
            &[event(1000, 4.0, 9e5, 10, 1)],
        );
        fs::create_dir_all(d.0.join("sessions")).unwrap();
        std::os::unix::fs::symlink(&real, d.0.join("sessions/rollout-l.jsonl")).unwrap();
        let s = load(&d.0.join("sessions"), 0.0);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].d7, Some(4.0));
    }

    #[test]
    fn jittered_resets_stay_one_analysis_segment() {
        let d = TempDir::new("jitter");
        write(
            &d.0,
            "rollout-a.jsonl",
            &[
                event(0, 90.0, 5_000.0, 0, 0), // an earlier window
                event(10_000, 0.0, 700_000.0, 100, 10),
                event(11_800, 1.0, 700_004.0, 200, 20),
                event(13_600, 2.0, 699_998.0, 300, 30),
            ],
        );
        let s = load(&d.0, 0.0);
        assert_eq!(s[1].d7r, s[3].d7r);
        assert_ne!(s[0].d7r, s[1].d7r);

        let a = analyze(&s, Win::SevenDay, 7200.0, 14_000.0).unwrap();
        assert_eq!(a.cur, 2.0);
        // 0 → 2% over an hour; with an exact-match segment only the last point would count
        assert!((a.rate.unwrap() - 2.0).abs() < 1e-9, "{:?}", a.rate);
        let tok_per_pct = a.conv.unwrap().tok_per_pct.unwrap();
        assert!((tok_per_pct - 110.0).abs() < 1e-9, "{tok_per_pct}");
    }

    fn tok(sid: &str, t: i64, tin: i64, tout: i64) -> Sample {
        Sample {
            t,
            sid: Some(sid.into()),
            tin: Some(tin),
            tout: Some(tout),
            ..Default::default()
        }
    }

    #[test]
    fn token_burn_nets_out_pre_window_totals() {
        let s = vec![
            tok("long", 100, 1_000, 100),
            tok("long", 900, 1_500, 150), // in window: 500 in, 50 out
            tok("fresh", 950, 40, 4),     // started in window: counts from zero
            tok("idle", 200, 9_000, 900), // nothing in window
            Sample {
                t: 960,
                sid: Some("long".into()),
                d7: Some(1.0),
                ..Default::default()
            },
        ];
        let b = token_burn(&s, 500.0, 1_000.0).unwrap();
        assert_eq!((b.tin, b.tout), (540.0, 54.0));
        assert!(token_burn(&s, 50.0, 2_000.0).is_none());
    }

    #[test]
    fn token_burn_takes_the_last_total_on_each_side() {
        let s = vec![
            tok("a", 100, 100, 10),
            tok("a", 300, 300, 30),
            tok("a", 700, 500, 50),
            tok("a", 900, 600, 60),
        ];
        let b = token_burn(&s, 500.0, 1_000.0).unwrap();
        assert_eq!((b.tin, b.tout), (300.0, 30.0));
    }

    fn week(t: i64, pct: f64, reset: f64) -> Sample {
        Sample {
            t,
            d7: Some(pct),
            d7r: Some(reset),
            ..Default::default()
        }
    }

    #[test]
    fn stale_burst_is_idle() {
        let reset = 700_000.0;
        let burst = vec![
            week(0, 0.0, reset),
            week(1_800, 9.0, reset),
            week(3_600, 18.0, reset),
        ];
        let later = 3_600.0 + 3.0 * 86_400.0;
        let a = analyze(&burst, Win::SevenDay, 7200.0, later).unwrap();
        // analyze alone still reports the old burst as a rate
        assert!(a.rate.is_some_and(|r| r > 0.0), "{:?}", a.rate);
        assert_eq!(recent_rate(&burst, &a, 7200.0, later), None);

        let mut one_recent = burst.clone();
        one_recent.push(week(later as i64 - 60, 18.0, reset));
        let a = analyze(&one_recent, Win::SevenDay, 7200.0, later).unwrap();
        assert_eq!(recent_rate(&one_recent, &a, 7200.0, later), None);

        let a = analyze(&burst, Win::SevenDay, 7200.0, 4_000.0).unwrap();
        assert!(a.rate.is_some());
        assert_eq!(recent_rate(&burst, &a, 7200.0, 4_000.0), a.rate);
    }

    #[test]
    fn a_trailing_windowless_event_keeps_the_week() {
        let d = TempDir::new("windowless");
        let last = serde_json::json!({
            "timestamp": ts(2000), "type": "event_msg",
            "payload": {"type": "token_count",
                "info": {"total_token_usage": {"input_tokens": 500, "output_tokens": 5}},
                "rate_limits": {"primary": null, "secondary": null}}
        });
        write(
            &d.0,
            "rollout-a.jsonl",
            &[event(1000, 4.0, 9e5, 100, 1), last.to_string()],
        );
        let s = load(&d.0, 0.0);
        assert_eq!(s.last().unwrap().d7, None);
        assert_eq!(analyze(&s, Win::SevenDay, 7200.0, 2100.0).unwrap().cur, 4.0);
    }
}

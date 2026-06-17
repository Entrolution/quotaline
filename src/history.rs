//! The usage-history log, shared with `report` and the inline burn-rate readout.
//!
//! Best-effort, throttled (~1/min), pruned, and written via atomic replace so the file is
//! never half-written. It is lock-free: the atomic rename prevents corruption, and because
//! the throttle is checked against the shared file, concurrent double-writes are rare and a
//! dropped sample is harmless. The on-disk schema matches the original Python tool's, so an
//! existing `usage-history.json` carries over unchanged.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json::{f64_at, get_pct, get_reset, nested};

pub const SAMPLE_INTERVAL: i64 = 60; // seconds; min gap between logged samples (global)
pub const MAX_ENTRIES: usize = 250; // prune to the last N samples
pub const RATE_WINDOW_MIN: f64 = 120.0; // trailing window for the inline burn rate

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Sample {
    #[serde(default)]
    pub t: i64,
    #[serde(default)]
    pub h5: Option<f64>,
    #[serde(default)]
    pub d7: Option<f64>,
    #[serde(default)]
    pub h5r: Option<f64>,
    #[serde(default)]
    pub d7r: Option<f64>,
    #[serde(default)]
    pub sid: Option<String>,
    #[serde(default)]
    pub usd: Option<f64>,
    #[serde(default)]
    pub tin: Option<i64>,
    #[serde(default)]
    pub tout: Option<i64>,
}

pub fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Where the history/state lives: `$CTT_STATE_DIR`, else `~/.claude/quotaline`.
pub fn state_dir() -> PathBuf {
    if let Some(d) = env::var_os("CTT_STATE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    match home_dir() {
        Some(home) => home.join(".claude").join("quotaline"),
        None => PathBuf::from(".state"),
    }
}

fn history_path(dir: &Path) -> PathBuf {
    dir.join("usage-history.json")
}

/// Read the history, returning an empty vec on any error (matches the Python's tolerance).
pub fn read_history(dir: &Path) -> Vec<Sample> {
    match fs::read_to_string(history_path(dir)) {
        Ok(s) => serde_json::from_str::<Vec<Sample>>(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Append one account-usage sample. Throttled, pruned, atomic. Best-effort (errors ignored).
pub fn log_sample(dir: &Path, input: &Value, now: i64) {
    // Only log once rate_limits is present (matches the Python: no sample on the n/a path).
    let has_limits =
        matches!(nested(input, &["rate_limits"]), Some(Value::Object(m)) if !m.is_empty());
    if !has_limits {
        return;
    }
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut hist = read_history(dir);
    if let Some(last) = hist.last() {
        if now - last.t < SAMPLE_INTERVAL {
            return; // too soon since the last sample
        }
    }

    let five = nested(input, &["rate_limits", "five_hour"]);
    let week = nested(input, &["rate_limits", "seven_day"]);
    let sid = input
        .get("session_id")
        .and_then(|x| x.as_str())
        .map(|s| s.chars().take(12).collect::<String>())
        .filter(|s| !s.is_empty());

    hist.push(Sample {
        t: now,
        h5: five.and_then(get_pct),
        d7: week.and_then(get_pct),
        h5r: five.and_then(get_reset),
        d7r: week.and_then(get_reset),
        sid,
        usd: f64_at(input, &["cost", "total_cost_usd"]),
        tin: nested(input, &["context_window", "total_input_tokens"]).and_then(|x| x.as_i64()),
        tout: nested(input, &["context_window", "total_output_tokens"]).and_then(|x| x.as_i64()),
    });

    if hist.len() > MAX_ENTRIES {
        let drop = hist.len() - MAX_ENTRIES;
        hist.drain(0..drop);
    }

    let json = match serde_json::to_string(&hist) {
        Ok(j) => j,
        Err(_) => return,
    };
    // Per-process temp name so concurrent sessions never clobber each other's write; the
    // rename is atomic, so the worst case is a dropped sample, never a corrupt file.
    let tmp = dir.join(format!("usage-history.{}.json.tmp", std::process::id()));
    if fs::write(&tmp, json).is_ok() {
        if fs::rename(&tmp, history_path(dir)).is_err() {
            let _ = fs::remove_file(&tmp); // don't leave an orphan temp on rename failure
        }
    } else {
        let _ = fs::remove_file(&tmp);
    }
}

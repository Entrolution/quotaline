//! quotaline — a Claude Code status line (and report) for account-wide usage limits.
//!
//!   quotaline                      read the stdin payload, render the status line
//!   quotaline report [--window N]  on-demand burn-rate + headroom report
//!
//! Data comes only from the JSON Claude Code pipes to status-line scripts on stdin —
//! no network, no auth, no Terms-of-Service surface.

use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

mod bars;
mod burn;
mod fmt;
mod history;
mod json;
mod report;
mod statusline;

/// Current Unix time in seconds (fractional). 0 if the clock is before the epoch.
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("report") => {
            let window = parse_window(&args).unwrap_or(report::DEFAULT_WINDOW_MIN);
            std::process::exit(report::run(window));
        }
        _ => run_statusline(),
    }
}

fn parse_window(args: &[String]) -> Option<f64> {
    let idx = args.iter().position(|a| a == "--window")?;
    args.get(idx + 1)?.parse::<f64>().ok()
}

/// The status-line path: read stdin, render, print, then log a sample. Each stage is wrapped
/// so a panic can never break the prompt (the analogue of the Python's top-level try/except).
fn run_statusline() {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input: serde_json::Value = if raw.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null)
    };
    let now = now_secs();

    let out = std::panic::catch_unwind(|| statusline::render(&input, now)).unwrap_or_default();
    {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = lock.write_all(out.as_bytes());
        let _ = lock.flush();
    }

    // After flush, so it can never delay the line.
    let _ = std::panic::catch_unwind(|| {
        history::log_sample(&history::state_dir(), &input, now.round() as i64);
    });
}

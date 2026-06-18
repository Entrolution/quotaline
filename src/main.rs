//! quotaline — a Claude Code status line (and report) for account-wide usage limits.
//!
//!   quotaline                          render the status line (reads JSON on stdin)
//!   quotaline report [--window N]      on-demand burn-rate + headroom report
//!   quotaline install [--refresh N]    wire into ~/.claude/settings.json
//!   quotaline uninstall                remove it again
//!
//! Status-line data comes only from the JSON Claude Code pipes on stdin —
//! no network, no auth, no Terms-of-Service surface.

use std::io::{IsTerminal, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

mod bars;
mod burn;
mod fmt;
mod history;
mod install;
mod json;
mod localtime;
mod memory;
mod report;
mod statusline;

const USAGE: &str = "\
quotaline — Claude Code usage status line

USAGE:
  quotaline                        render the status line (reads Claude Code's JSON on stdin)
  quotaline report [--window N]    on-demand burn-rate + headroom report (N minutes)
  quotaline install [--refresh N]  wire into ~/.claude/settings.json (refresh seconds; default 10)
  quotaline uninstall              remove the statusLine block
";

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
        None => run_statusline(),
        Some("report") => {
            let window = flag_f64(&args, "--window").unwrap_or(report::DEFAULT_WINDOW_MIN);
            std::process::exit(report::run(window));
        }
        Some("install") => {
            let refresh = flag_u64(&args, "--refresh").unwrap_or(10);
            std::process::exit(install::install(refresh));
        }
        Some("uninstall") => std::process::exit(install::uninstall()),
        Some("-h") | Some("--help") | Some("help") => print!("{USAGE}"),
        Some(other) => {
            eprintln!("quotaline: unknown command '{other}'\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let idx = args.iter().position(|a| a == name)?;
    args.get(idx + 1).map(String::as_str)
}

fn flag_f64(args: &[String], name: &str) -> Option<f64> {
    flag_value(args, name)?.parse().ok()
}

fn flag_u64(args: &[String], name: &str) -> Option<u64> {
    flag_value(args, name)?.parse().ok()
}

/// The status-line path: read stdin, render, print, then log a sample. Each stage is wrapped
/// so a panic can never break the prompt (the analogue of the Python's top-level try/except).
fn run_statusline() {
    // Invoked interactively with no piped input? Show usage rather than blocking on stdin.
    if std::io::stdin().is_terminal() {
        print!("{USAGE}");
        return;
    }

    // Silent fallback: drop the default panic hook's stderr message. The catch_unwind below
    // keeps the prompt alive; this just avoids leaking panic noise to the host.
    std::panic::set_hook(Box::new(|_| {}));

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

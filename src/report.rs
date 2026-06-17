//! `quotaline report [--window N]` — the on-demand burn-rate + headroom report
//! (a port of the original burn.py).

use crate::burn::{analyze, Win};
use crate::fmt::{color_for, fmt_dur, group_thousands, BOLD, DIM, GRAY, GREEN, RED, RESET};
use crate::history::{read_history, state_dir};

pub const DEFAULT_WINDOW_MIN: f64 = 120.0;

fn simple_bar(pct: f64, width: usize) -> String {
    let p = pct.clamp(0.0, 100.0);
    let fill = (((p / 100.0) * width as f64).round() as usize).min(width);
    format!(
        "{}{}{GRAY}{}{RESET}",
        color_for(Some(p)),
        "█".repeat(fill),
        "░".repeat(width - fill)
    )
}

pub fn run(window_min: f64) -> i32 {
    let hist = read_history(&state_dir());
    let now = crate::now_secs();
    if hist.len() < 2 {
        println!(
            "Not enough samples yet ({}). The status line logs ~1/min while sessions are \
             active — check back in a few minutes.",
            hist.len()
        );
        return 0;
    }

    let span = fmt_dur((hist[hist.len() - 1].t - hist[0].t).max(0));
    println!(
        "{BOLD}Claude usage — burn rate{RESET}{DIM}  ({} samples over {span}){RESET}",
        hist.len()
    );
    println!();

    for (label, win) in [("5h", Win::FiveHour), ("wk", Win::SevenDay)] {
        let a = match analyze(&hist, win, window_min * 60.0, now) {
            Some(a) => a,
            None => continue,
        };
        let cur = a.cur;
        let mut line = format!(
            "  {BOLD}{label}{RESET}  {}  {}{cur:>3.0}%{RESET}",
            simple_bar(cur, 14),
            color_for(Some(cur))
        );

        let burning = a.rate.map(|r| r > 0.05).unwrap_or(false);
        if cur >= 100.0 {
            line.push_str(&format!("  {RED}AT LIMIT{RESET}"));
        } else if burning {
            let rate = a.rate.unwrap();
            let eta = (100.0 - cur) / rate * 3600.0;
            line.push_str(&format!("  {rate:+.1}%/hr   ETA {}", fmt_dur(eta as i64)));
        } else {
            line.push_str(&format!("  {DIM}~idle (no measurable burn){RESET}"));
        }

        if let Some(reset) = a.reset {
            let ttr = reset - now;
            line.push_str(&format!("{DIM}   resets in {}{RESET}", fmt_dur(ttr as i64)));
            if burning && cur < 100.0 {
                let eta = (100.0 - cur) / a.rate.unwrap() * 3600.0;
                if eta < ttr {
                    line.push_str(&format!("  {RED}→ hits cap first{RESET}"));
                } else {
                    line.push_str(&format!("  {GREEN}→ resets first{RESET}"));
                }
            }
        }
        println!("{line}");

        // headroom (cost-anchored, approximate)
        match a.conv.as_ref().and_then(|c| c.usd_per_pct.map(|u| (c, u))) {
            Some((conv, usd_per_pct)) => {
                let head = (100.0 - cur) * usd_per_pct;
                let mut extra = format!("      {DIM}headroom ~${head:.2}   (${usd_per_pct:.3}/1%");
                if let Some(tok) = conv.tok_per_pct {
                    extra.push_str(&format!(", ≈{} raw-tok/1%", group_thousands(tok)));
                }
                println!("{extra}){RESET}");
            }
            None => {
                println!(
                    "      {DIM}headroom: n/a — need more % movement to anchor a $/% estimate{RESET}"
                );
            }
        }
        println!();
    }

    println!("{DIM}  % and ETA are exact (account-wide). $/token are estimates — they assume your{RESET}");
    println!(
        "{DIM}  usage is mostly these sessions; claude.ai etc. moves % but isn't logged.{RESET}"
    );
    println!(
        "{DIM}  raw-tok counts include cache re-reads, so the $ figure is the steadier one.{RESET}"
    );
    0
}

//! `quotaline report [--window N]` — the on-demand burn-rate + headroom report
//! (a port of the original burn.py), plus a real-$ day section for API-key sessions.

use crate::burn::{analyze, Win};
use crate::fmt::{
    color_for, fmt_dur, fmt_usd, fmt_usd_rate, group_thousands, BOLD, DIM, GRAY, GREEN, RED, RESET,
};
use crate::history::{read_history, state_dir};
use crate::spend::{budget_outcome, day_rate, day_spend, BudgetOutcome};

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

    let mut sub_shown = false;
    for (label, win) in [("5h", Win::FiveHour), ("wk", Win::SevenDay)] {
        let a = match analyze(&hist, win, window_min * 60.0, now) {
            Some(a) => a,
            None => continue,
        };
        sub_shown = true;
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
            let clock = crate::fmt::fmt_clock(reset)
                .map(|c| format!(" @ {c}"))
                .unwrap_or_default();
            line.push_str(&format!(
                "{DIM}   resets in {}{clock}{RESET}",
                fmt_dur(ttr as i64)
            ));
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

    let api_shown = api_section(&hist, window_min, now);

    if sub_shown {
        println!("{DIM}  % and ETA are exact (account-wide). $/token are estimates — they assume your{RESET}");
        println!(
            "{DIM}  usage is mostly these sessions; claude.ai etc. moves % but isn't logged.{RESET}"
        );
        println!(
            "{DIM}  raw-tok counts include cache re-reads, so the $ figure is the steadier one.{RESET}"
        );
    }
    if api_shown {
        println!(
            "{DIM}  day $ covers this machine's Claude Code API-key sessions only, from logged{RESET}"
        );
        println!(
            "{DIM}  samples — the status line's live figure can run up to a minute ahead.{RESET}"
        );
    }
    if !sub_shown && !api_shown {
        println!(
            "{DIM}  The samples carry no usable window percentages or API-key spend yet —{RESET}"
        );
        println!("{DIM}  nothing to analyse. Leave a session running and check back.{RESET}");
    }
    0
}

/// The API-key day section: $ spent today across sessions, the recent $/hr rate, and —
/// when `QUOTALINE_DAILY_BUDGET` is set — how spend stands against the budget. Printed only
/// when API-key spend was attributed to today; returns whether it printed.
fn api_section(hist: &[crate::history::Sample], window_min: f64, now: f64) -> bool {
    let (midnight, next_mid) = crate::localtime::day_bounds(now);
    let day = day_spend(hist, midnight, None, now);
    if day.sessions == 0 {
        return false;
    }
    let rate = day_rate(hist, None, window_min * 60.0, now);

    let sessions = if day.sessions == 1 {
        "1 session".to_string()
    } else {
        format!("{} sessions", day.sessions)
    };
    let mut line = format!(
        "  {BOLD}day{RESET}  {BOLD}{}{RESET} today {DIM}({sessions}){RESET}",
        fmt_usd(day.total)
    );
    match rate {
        Some(r) if r.per_hr > crate::spend::IDLE_RATE_PER_HR => {
            line.push_str(&format!("   +{}/hr", fmt_usd_rate(r.per_hr)))
        }
        _ => line.push_str(&format!("   {DIM}~idle (no measurable burn){RESET}")),
    }
    let clock = crate::fmt::fmt_clock(next_mid)
        .map(|c| format!(" @ {c}"))
        .unwrap_or_default();
    line.push_str(&format!(
        "{DIM}   rolls over in {}{clock}{RESET}",
        fmt_dur((next_mid - now) as i64)
    ));
    println!("{line}");

    match crate::spend::daily_budget() {
        Some(b) => {
            let pct = day.total / b * 100.0;
            let mut extra = format!(
                "      {DIM}budget {}/day — {}{:.0}%{RESET}{DIM} used{RESET}",
                fmt_usd(b),
                color_for(Some(pct)),
                pct
            );
            match budget_outcome(day.total, b, rate, next_mid, now) {
                BudgetOutcome::Over => {
                    extra.push_str(&format!(" {RED}— over budget{RESET}"));
                }
                BudgetOutcome::HitsBudget { eta_secs } => {
                    extra.push_str(&format!(
                        "{DIM}, ETA {}{RESET} {RED}→ hits budget before midnight{RESET}",
                        fmt_dur(eta_secs as i64)
                    ));
                }
                BudgetOutcome::RollsOverFirst => {
                    extra.push_str(&format!(" {GREEN}→ rolls over first{RESET}"));
                }
            }
            println!("{extra}");
        }
        // The status line has no error channel, but the report does: say when a set
        // budget was rejected (e.g. `$50` instead of `50`) instead of silently hiding it.
        None if crate::spend::budget_var_set() => {
            println!(
                "      {DIM}QUOTALINE_DAILY_BUDGET is set but not a positive number \
                 (use e.g. 50, not $50) — ignored.{RESET}"
            );
        }
        None => {}
    }
    println!();
    true
}

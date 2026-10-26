//! `quotaline report [--window N]` — the on-demand burn-rate + headroom report
//! (a port of the original burn.py), plus a real-$ day section for API-key sessions.
//! `--codex` reports OpenAI Codex plan usage instead (see `codex`).

use crate::burn::{analyze, Analysis, Win};
use crate::fmt::{
    color_for, fmt_dur, fmt_tokens, fmt_usd, fmt_usd_rate, group_thousands, BOLD, DIM, GRAY, GREEN,
    RED, RESET,
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

    let over = over_span(hist[hist.len() - 1].t - hist[0].t);
    println!(
        "{BOLD}Claude usage — burn rate{RESET}{DIM}  ({} samples{over}){RESET}",
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
        println!("{}", window_line(label, &a, now));

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

/// One window's bar, %, burn rate with cap ETA, and reset countdown.
fn window_line(label: &str, a: &Analysis, now: f64) -> String {
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
    line
}

/// `" over <span>"`, or nothing for a span that isn't positive; see
/// `codex_header_stats_never_says_now_ago`.
fn over_span(span_secs: i64) -> String {
    if span_secs > 0 {
        format!(" over {}", fmt_dur(span_secs))
    } else {
        String::new()
    }
}

/// A newest event stamped in the future (clock skew) gets no "last …" clause rather than a
/// time that can't be true.
fn codex_header_stats(events: usize, span_secs: i64, since_last_secs: i64) -> String {
    let noun = if events == 1 { "event" } else { "events" };
    let mut s = format!(
        "{} {noun}{}",
        group_thousands(events as f64),
        over_span(span_secs)
    );
    if since_last_secs > 0 {
        s.push_str(&format!(", last {} ago", fmt_dur(since_last_secs)));
    } else if since_last_secs == 0 {
        s.push_str(", last just now");
    }
    s
}

/// `quotaline report --codex`: the weekly window and token burn rebuilt from Codex's own
/// rollout logs. No $ section: plan usage is not metered in dollars.
pub fn run_codex(window_min: f64) -> i32 {
    let now = crate::now_secs();
    let window_sec = window_min * 60.0;
    let Some(home) = crate::codex::codex_home() else {
        eprintln!("error: can't locate Codex's state: set CODEX_HOME or HOME.");
        return 1;
    };
    let sessions = home.join("sessions");
    let missing = match std::fs::metadata(&sessions) {
        Ok(m) => !m.is_dir(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            eprintln!("error: can't read {}: {e}", sessions.display());
            return 1;
        }
    };
    if missing {
        if std::env::var_os("CODEX_HOME").is_some_and(|v| !v.is_empty()) {
            eprintln!(
                "error: CODEX_HOME has no sessions directory: {}",
                sessions.display()
            );
            return 1;
        }
        println!(
            "No Codex sessions under {}. Set CODEX_HOME if Codex keeps its state elsewhere.",
            sessions.display()
        );
        return 0;
    }
    if let Err(e) = std::fs::read_dir(&sessions) {
        eprintln!("error: can't read {}: {e}", sessions.display());
        return 1;
    }
    let lookback = crate::codex::WEEK_SECS.max(window_sec);
    let hist = crate::codex::load(&sessions, now - lookback);
    let Some(last) = hist.last() else {
        println!(
            "Found no readable Codex token events in rollouts modified in the last {} under {}.",
            fmt_dur(lookback as i64),
            sessions.display()
        );
        return 0;
    };

    let stats = codex_header_stats(hist.len(), last.t - hist[0].t, (now - last.t as f64) as i64);
    println!("{BOLD}Codex usage — burn rate{RESET}{DIM}  ({stats}){RESET}");
    println!();

    match analyze(&hist, Win::SevenDay, window_sec, now) {
        Some(mut a) => {
            a.rate = crate::codex::recent_rate(&hist, &a, window_sec, now);
            println!("{}", window_line("wk", &a, now));
            match a.conv.as_ref().and_then(|c| c.tok_per_pct) {
                Some(tok) => println!("      {DIM}≈{} raw-tok/1%{RESET}", group_thousands(tok)),
                None => println!(
                    "      {DIM}raw-tok/1%: n/a — need more % movement to anchor it{RESET}"
                ),
            }
        }
        None => println!("  {BOLD}wk{RESET}  {DIM}no current weekly window reported{RESET}"),
    }
    println!();

    let window = fmt_dur(window_sec as i64);
    match crate::codex::token_burn(&hist, window_sec, now) {
        Some(b) => {
            let per_hr = (b.tin + b.tout) / (window_min / 60.0);
            println!(
                "  {BOLD}tok{RESET}  in {} · out {} over the last {window}   ≈{}/hr",
                fmt_tokens(b.tin),
                fmt_tokens(b.tout),
                fmt_tokens(per_hr)
            );
        }
        None => println!("  {BOLD}tok{RESET}  {DIM}~idle (no tokens in the last {window}){RESET}"),
    }
    println!();

    println!(
        "{DIM}  % is account-wide, as of Codex's last recorded event. Tokens count only{RESET}"
    );
    println!(
        "{DIM}  rollouts under {}, so other machines move % unseen.{RESET}",
        home.display()
    );
    println!("{DIM}  raw-tok counts include cached input re-reads.{RESET}");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line(cur: f64, rate: Option<f64>, reset_in: f64) -> String {
        let a = Analysis {
            cur,
            reset: Some(1_000.0 + reset_in),
            rate,
            conv: None,
        };
        window_line("wk", &a, 1_000.0)
    }

    #[test]
    fn window_line_verdicts() {
        assert!(line(100.0, Some(5.0), 3_600.0).contains("AT LIMIT"));
        // 50% left at 10%/hr is 5h
        assert!(line(50.0, Some(10.0), 36_000.0).contains("hits cap first"));
        assert!(line(50.0, Some(10.0), 3_600.0).contains("resets first"));
        let idle = line(50.0, None, 3_600.0);
        assert!(idle.contains("~idle") && !idle.contains("first"), "{idle}");
    }

    #[test]
    fn codex_header_stats_never_says_now_ago() {
        assert_eq!(
            codex_header_stats(8_640, 601_200, 0),
            "8,640 events over 6d23h, last just now"
        );
        assert_eq!(codex_header_stats(1, 0, 180), "1 event, last 3m ago");
        assert_eq!(
            codex_header_stats(2, 30, 30),
            "2 events over <1m, last <1m ago"
        );
        assert_eq!(codex_header_stats(3, 120, -5), "3 events over 2m");
        assert_eq!(over_span(-5), "");
    }
}

//! Assembles the three-line status line (header + 5h/weekly bars) from the stdin payload.

use serde_json::Value;

use crate::bars::framed;
use crate::burn::{burn_suffix, history_rate, Win};
use crate::fmt::{color_for, fmt_dur, fmt_tokens, AMBER, DIM, GRAY, GREEN, RED, RESET};
use crate::history::{read_history, state_dir};
use crate::json::{f64_at, get_pct, get_reset, nested, str_at};

const MIN_BAR: usize = 8;
const MAX_BAR: usize = 150;
const SAFE_MARGIN: usize = 8;
const FALLBACK_COLS: usize = 80;
// per-line fixed overhead: label(2) + 2sp + frame(1) + frame(1) + sp(1) + pct(3) + 2sp
const LINE_OVERHEAD: usize = 12;

const CTX_AMBER_TOK: f64 = 200_000.0;
const CTX_RED_TOK: f64 = 500_000.0;

fn term_width() -> usize {
    if let Some(c) = std::env::var_os("COLUMNS") {
        if let Some(s) = c.to_str() {
            if let Ok(n) = s.trim().parse::<usize>() {
                if n > 0 {
                    return n;
                }
            }
        }
    }
    FALLBACK_COLS
}

/// Colour the ctx value by the more severe of absolute size (cost) and % full (window risk).
fn ctx_color(abs_tok: Option<f64>, pct: Option<f64>) -> &'static str {
    let mut level = 0u8;
    if let Some(t) = abs_tok {
        if t >= CTX_RED_TOK {
            level = 2;
        } else if t >= CTX_AMBER_TOK {
            level = 1;
        }
    }
    if let Some(p) = pct {
        if p >= 90.0 {
            level = 2;
        } else if p >= 70.0 && level < 1 {
            level = 1;
        }
    }
    match level {
        2 => RED,
        1 => AMBER,
        _ => GREEN,
    }
}

/// Compact `Model · effort: level · ctx N% (size)` header; `None` if all absent.
fn header(input: &Value) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(model) = str_at(input, &["model", "display_name"]) {
        parts.push(format!("{DIM}{model}{RESET}"));
    }
    if let Some(level) = str_at(input, &["effort", "level"]) {
        parts.push(format!("{DIM}effort: {level}{RESET}"));
    }
    if let Some(pct) = f64_at(input, &["context_window", "used_percentage"]) {
        let mut abs_tok = f64_at(input, &["context_window", "total_input_tokens"]);
        if abs_tok.is_none() {
            if let Some(size) = f64_at(input, &["context_window", "context_window_size"]) {
                abs_tok = Some(pct / 100.0 * size);
            }
        }
        let mut val = format!("{}%", pct.round() as i64);
        if let Some(t) = abs_tok {
            if t > 0.0 {
                val.push_str(&format!(" ({})", fmt_tokens(t)));
            }
        }
        parts.push(format!(
            "{DIM}ctx {RESET}{}{val}{RESET}",
            ctx_color(abs_tok, Some(pct))
        ));
    }
    if let Some(stat) = crate::memory::measure(str_at(input, &["transcript_path"])) {
        parts.push(crate::memory::header_segment(&stat));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(&format!("{DIM} · {RESET}")))
    }
}

fn reset_text(reset: Option<f64>, now: f64) -> String {
    match reset {
        Some(e) => fmt_dur((e - now) as i64),
        None => "—".to_string(),
    }
}

fn render_line(
    label: &str,
    pct: Option<f64>,
    bar_width: usize,
    rt: &str,
    suffix_colored: &str,
) -> String {
    let pctf = match pct {
        Some(p) => format!("{:>2}%", p.round() as i64),
        None => "--%".to_string(),
    };
    let mut s = format!(
        "{DIM}{label}{RESET}  {bar} {pc}{pctf}{RESET}  {DIM}{rt}{RESET}",
        bar = framed(pct, bar_width),
        pc = color_for(pct),
    );
    if !suffix_colored.is_empty() {
        s.push_str("  ");
        s.push_str(suffix_colored);
    }
    s
}

/// Plain (uncoloured) right-hand text, for width measurement only.
fn join_right(rt: &str, suffix_plain: &str) -> String {
    if suffix_plain.is_empty() {
        rt.to_string()
    } else {
        format!("{rt}  {suffix_plain}")
    }
}

/// Render the full status line (no trailing newline). Pure — logging happens after, in main.
pub fn render(input: &Value, now: f64) -> String {
    let rl = nested(input, &["rate_limits"]);
    let present = matches!(rl, Some(Value::Object(m)) if !m.is_empty());
    if !present {
        return format!("{GRAY}limits n/a (awaiting first API response){RESET}");
    }
    let rl = rl.unwrap();
    let five = rl.get("five_hour").filter(|v| !v.is_null());
    let week = rl.get("seven_day").filter(|v| !v.is_null());

    let hist = read_history(&state_dir());

    let five_pct = five.and_then(get_pct);
    let five_reset = five.and_then(get_reset);
    let week_pct = week.and_then(get_pct);
    let week_reset = week.and_then(get_reset);

    let (five_sp, five_sc) = burn_suffix(
        five_pct,
        five_reset,
        history_rate(&hist, Win::FiveHour, five_reset, now),
        now,
    );
    let (week_sp, week_sc) = burn_suffix(
        week_pct,
        week_reset,
        history_rate(&hist, Win::SevenDay, week_reset, now),
        now,
    );

    let rt5 = reset_text(five_reset, now);
    let rt7 = reset_text(week_reset, now);
    let max_right = join_right(&rt5, &five_sp)
        .chars()
        .count()
        .max(join_right(&rt7, &week_sp).chars().count());

    let width = term_width();
    let avail = width as i64 - SAFE_MARGIN as i64 - LINE_OVERHEAD as i64 - max_right as i64;
    let bar_width = avail.clamp(MIN_BAR as i64, MAX_BAR as i64) as usize;

    let mut lines: Vec<String> = Vec::new();
    if let Some(h) = header(input) {
        lines.push(h);
    }
    lines.push(render_line("5h", five_pct, bar_width, &rt5, &five_sc));
    lines.push(render_line("wk", week_pct, bar_width, &rt7, &week_sc));
    lines.join("\n")
}

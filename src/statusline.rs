//! Assembles the status line from the stdin payload: header plus 5h/weekly bars on a
//! subscription, or header plus a real-$ "spent today" line on API-key billing.

use serde_json::Value;

use crate::bars::framed;
use crate::burn::{burn_suffix, history_rate, Win};
use crate::fmt::{color_for, fmt_dur, fmt_tokens, fmt_usd, AMBER, DIM, GRAY, GREEN, RED, RESET};
use crate::history::{read_history, state_dir, Sample, RATE_WINDOW_MIN};
use crate::json::{f64_at, get_pct, get_reset, nested, payload_mode, str_at, Mode};
use crate::spend::{day_rate, day_spend, spend_suffix, Live};

const MIN_BAR: usize = 8;
const MAX_BAR: usize = 150;
const SAFE_MARGIN: usize = 8;
const FALLBACK_COLS: usize = 80;
// per-line fixed overhead around the bar, excluding the label: 2sp + frame(1) + frame(1) +
// sp(1) + pct(3) + 2sp
const LINE_OVERHEAD: usize = 10;

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
/// `session_usd` (API-key mode only) appends the session's real billed cost.
fn header(input: &Value, session_usd: Option<f64>) -> Option<String> {
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
    if let Some(u) = session_usd {
        parts.push(fmt_usd(u));
    }
    // `mem` then `int`, matching the direction of flow: Claude writes to MEMORY.md as an
    // inbox and `/dream` drains it into the curated intuition.md. On a migrated store a large
    // `mem` therefore means "the inbox needs draining", a different and more actionable
    // signal than the old "the index is about to truncate".
    if let Some(stat) = crate::memory::measure(str_at(input, &["transcript_path"])) {
        parts.push(crate::memory::header_segment(&stat));
    }
    if let Some(stat) = crate::memory::measure_intuition(str_at(input, &["transcript_path"])) {
        parts.push(crate::memory::intuition_header_segment(&stat));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(&format!("{DIM} · {RESET}")))
    }
}

/// The dim reset readout: the time-to-reset duration, plus the absolute local clock it lands at
/// (e.g. `6d3h @ 12am (Wed)`). The clock is dropped only if local time can't be resolved.
fn reset_text(reset: Option<f64>, now: f64) -> String {
    match reset {
        Some(e) => {
            let dur = fmt_dur((e - now) as i64);
            match crate::fmt::fmt_clock(e) {
                Some(clock) => format!("{dur} @ {clock}"),
                None => dur,
            }
        }
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

/// Render the full status line (no trailing newline). Pure aside from reading the sample
/// history and env — logging happens after, in main.
pub fn render(input: &Value, now: f64) -> String {
    match payload_mode(input) {
        Mode::Subscription => render_subscription(input, now),
        Mode::ApiKey => render_api_key(input, now),
        Mode::Unknown => limits_na(),
    }
}

fn limits_na() -> String {
    format!("{GRAY}limits n/a (awaiting first API response){RESET}")
}

/// The session *has* reported subscription windows and they have stopped arriving, with the
/// switch to API-key billing not yet proven. Never-seen and seen-and-lost are different
/// states and must read differently: after hours of responses, "awaiting first API
/// response" is simply false. The header is kept because this state can now last as long as
/// [`crate::history::SUB_HOLD_SECS`], where the resume transient it was written for lasted
/// seconds — but the cost stays out of it, since an unproven counter may still be a
/// subscriber's shadow estimate rather than money.
fn limits_held(input: &Value, quiet_secs: i64) -> String {
    // A duration is only quoted when it can be true: a clock that stepped backwards (or a
    // sample stored under a wrong one) yields a negative or absurd figure, and `fmt_dur`
    // would render the negative case as "none reported for now". The ceiling is the weekly
    // window rather than the retention age — a subscription-only history is pruned by count
    // alone, so a session resumed a couple of days later reports a long but honest quiet.
    const QUOTABLE: i64 = 7 * 24 * 3600;
    let msg = match quiet_secs {
        q if q > 0 && q <= QUOTABLE => {
            format!("{GRAY}limits n/a (none reported for {}){RESET}", fmt_dur(q))
        }
        _ => format!("{GRAY}limits n/a (none reported){RESET}"),
    };
    match header(input, None) {
        Some(h) => format!("{h}\n{msg}"),
        None => msg,
    }
}

/// Subscription (Pro/Max): the account-wide 5-hour and weekly bars.
fn render_subscription(input: &Value, now: f64) -> String {
    // payload_mode guarantees a non-empty rate_limits object here, but that invariant lives
    // in another module — degrade like every other missing-data path rather than trusting it.
    let Some(rl) = nested(input, &["rate_limits"]) else {
        return limits_na();
    };
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

    let bar_width = bar_width(2, max_right);

    let mut lines: Vec<String> = Vec::new();
    if let Some(h) = header(input, None) {
        lines.push(h);
    }
    lines.push(render_line("5h", five_pct, bar_width, &rt5, &five_sc));
    lines.push(render_line("wk", week_pct, bar_width, &rt7, &week_sc));
    lines.join("\n")
}

/// Bar cells available given the label width and the longest right-hand text.
fn bar_width(label_len: usize, max_right: usize) -> usize {
    let width = term_width();
    let avail =
        width as i64 - SAFE_MARGIN as i64 - (LINE_OVERHEAD + label_len) as i64 - max_right as i64;
    avail.clamp(MIN_BAR as i64, MAX_BAR as i64) as usize
}

/// API-key (pay-as-you-go): no account allowance exists, so show real dollars instead —
/// the session's cost in the header and a machine-wide "spent today" line beneath, with a
/// $/h burn rate and the local-midnight rollover (plus a budget bar when one is set).
fn render_api_key(input: &Value, now: f64) -> String {
    render_api_key_with(input, now, &read_history(&state_dir()))
}

/// The body of [`render_api_key`], with the history passed in so the classification
/// branches are reachable from a test without touching the process-wide environment.
fn render_api_key_with(input: &Value, now: f64, hist: &[Sample]) -> String {
    // Filter once at the source: everything below (header, live point) inherits it.
    let usd = f64_at(input, &["cost", "total_cost_usd"]).filter(|u| u.is_finite() && *u >= 0.0);
    let sid = str_at(input, &["session_id"]);
    // A resumed subscription session restores its cost before rate_limits repopulates and
    // so classifies as ApiKey for a moment; while the history still reads this sid as a
    // subscription session, hold the honest waiting line rather than presenting shadow cost
    // as spend. The hold lifts once the session's own cost counter proves the switch is
    // durable — without that expiry a permanent change of auth mid-session (`--resume`
    // reuses the session id) was mistaken for the transient forever.
    if let Some(k) = crate::history::session_key(sid) {
        if let Some(quiet) = crate::history::SubSessions::build(hist).hold(&k, usd, now as i64) {
            return limits_held(input, quiet);
        }
    }
    let (midnight, next_mid) = crate::localtime::day_bounds(now);

    let live = usd.map(|u| Live { sid, usd: u });
    let day = day_spend(hist, midnight, live.as_ref(), now);
    // No attributable session (e.g. a payload without a session id): day accounting is
    // impossible, so fall back like the report does rather than rendering an empty $0 line.
    if day.sessions == 0 {
        return limits_na();
    }
    let rate = day_rate(hist, live.as_ref(), RATE_WINDOW_MIN * 60.0, now);

    let budget = crate::spend::daily_budget();
    let (sp, sc) = spend_suffix(day.total, budget, rate, next_mid, now);
    let rt_reset = reset_text(Some(next_mid), now);

    let mut lines: Vec<String> = Vec::new();
    if let Some(h) = header(input, usd) {
        lines.push(h);
    }
    lines.push(match budget {
        Some(b) => {
            // Display-clamp the percentage: spend isn't capped at the budget, and an
            // unbounded value would blow the 3-char field the width maths reserves.
            let pct = (day.total / b * 100.0).min(999.0);
            let rt = format!("{} of {} · {}", fmt_usd(day.total), fmt_usd(b), rt_reset);
            let bw = bar_width(3, join_right(&rt, &sp).chars().count());
            render_line("day", Some(pct), bw, &rt, &sc)
        }
        None => {
            let mut s = format!(
                "{DIM}day{RESET}  {}  {DIM}{rt_reset}{RESET}",
                fmt_usd(day.total)
            );
            if !sc.is_empty() {
                s.push_str("  ");
                s.push_str(&sc);
            }
            s
        }
    });
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::SUB_HOLD_SECS;

    fn windowed(t: i64, usd: f64) -> Sample {
        Sample {
            t,
            h5: Some(7.0),
            h5r: Some(t as f64 + 3600.0),
            sid: Some("sess-abc-def-ghi".into()),
            usd: Some(usd),
            ..Default::default()
        }
    }

    fn payload(usd: f64) -> Value {
        serde_json::json!({
            "session_id": "sess-abc-def-ghi",
            "model": {"display_name": "Opus 5"},
            "cost": {"total_cost_usd": usd}
        })
    }

    #[test]
    fn a_held_session_says_the_windows_stopped_not_that_none_ever_came() {
        // Hours of subscription samples, then the payload arrives without them.
        let hist: Vec<Sample> = (0..5).map(|i| windowed(1_000 + i * 600, 28.19)).collect();
        let out = render_api_key_with(&payload(31.4), 3_400.0 + 5_400.0, &hist);
        assert!(out.contains("none reported for"), "{out}");
        assert!(!out.contains("awaiting first"), "{out}");
        assert!(
            out.contains("Opus 5"),
            "the header survives the hold: {out}"
        );
        // An unproven counter may still be a subscriber's shadow estimate, so no dollars.
        assert!(!out.contains("31.40") && !out.contains("day"), "{out}");
    }

    #[test]
    fn a_quiet_time_that_cannot_be_true_is_not_quoted() {
        let hist = vec![windowed(1_000, 28.19)];
        // Clock stepped back behind the sample: `fmt_dur` would render this as "now".
        let out = render_api_key_with(&payload(31.4), 900.0, &hist);
        assert!(out.contains("(none reported)"), "{out}");
        assert!(!out.contains("for now"), "{out}");
    }

    #[test]
    fn a_session_the_history_never_saw_on_a_subscription_is_not_held() {
        let out = render_api_key_with(&payload(31.4), 10_000.0, &[]);
        assert!(!out.contains("none reported"), "{out}");
    }

    #[test]
    fn a_proven_switch_renders_the_day_line() {
        let mut hist = vec![windowed(1_000, 28.19)];
        // Restored at $28.19, then seen climbing an hour before `now`.
        hist.push(Sample {
            t: 2_000,
            sid: Some("sess-abc-def-ghi".into()),
            usd: Some(28.19),
            ..Default::default()
        });
        hist.push(Sample {
            t: 2_600,
            sid: Some("sess-abc-def-ghi".into()),
            usd: Some(29.5),
            api: true,
            ..Default::default()
        });
        let out = render_api_key_with(&payload(31.4), (2_600 + SUB_HOLD_SECS) as f64, &hist);
        assert!(out.contains("day"), "{out}");
        assert!(
            out.contains("31.40"),
            "the session's real cost joins the header: {out}"
        );
    }

    /// Drives `log_sample` and the renderer together over a realistic timeline — the one
    /// interaction neither module's own tests can show, since each half is what the other
    /// half reads.
    #[test]
    fn a_switch_timeline_holds_then_releases_and_charges_only_post_switch_dollars() {
        let dir = std::env::temp_dir().join(format!("quotaline-timeline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let sub = |t: i64, usd: f64| {
            serde_json::json!({
                "session_id": "sess-abc-def-ghi",
                "cost": {"total_cost_usd": usd},
                "rate_limits": {"five_hour": {"used_percentage": 7, "resets_at": t + 3600}}
            })
        };

        // Two hours on the subscription, shadow cost climbing $0.10/min.
        let mut usd = 10.0;
        for i in 0..120 {
            usd += 0.1;
            crate::history::log_sample(&dir, &sub(i * 60, usd), i * 60);
        }
        let switch_at = 120 * 60;
        // Then resumed under an API key: same id, no windows, real spend at $0.20/min.
        let mut released_at = None;
        for i in 0..180 {
            let t = switch_at + i * 60;
            usd += 0.2;
            let p = payload(usd);
            let hist = crate::history::read_history(&dir);
            let out = render_api_key_with(&p, t as f64, &hist);
            if released_at.is_none() && out.contains("day") {
                released_at = Some(t);
            }
            crate::history::log_sample(&dir, &p, t);
        }

        let released = released_at.expect("a session spending for three hours is reclassified");
        // Held from the switch until 45 min after the first *movement* was observed, and no
        // longer: the first probe lands a minute in, the movement a minute after that.
        let elapsed = released - switch_at;
        assert!(
            (2760..=2940).contains(&elapsed),
            "released {elapsed}s after the switch"
        );

        // Only dollars spent after the release are charged — never the subscription hours,
        // whose shadow estimate rode the very same counter.
        let hist = crate::history::read_history(&dir);
        let day = crate::spend::day_spend(&hist, 0.0, None, (switch_at + 180 * 60) as f64);
        let post_release_minutes = (180 * 60 - (released - switch_at)) / 60;
        let charged_ceiling = post_release_minutes as f64 * 0.2;
        assert!(
            day.total <= charged_ceiling + 1e-9,
            "charged {} against a ceiling of {charged_ceiling} — shadow cost leaked",
            day.total
        );
        // …and all but the first interval of it is charged: the baseline is the session's
        // first observation after release, so exactly one sample of burn is dropped.
        assert!(
            day.total > charged_ceiling - 0.25,
            "charged only {} of {charged_ceiling}",
            day.total
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two memory gauges must appear together, in inbox-then-curated-store order, and the
    /// second must vanish entirely on a project that has no `intuition.md`. Neither the
    /// `memory` unit tests nor the other statusline tests cover this: those exercise the
    /// gauges in isolation, and every other payload here omits `transcript_path`, so the
    /// wiring added for the second gauge would otherwise ship unexercised.
    #[test]
    fn memory_gauges_render_in_order_and_only_when_their_files_exist() {
        let dir = std::env::temp_dir().join(format!("quotaline-sl-mem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join("proj");
        std::fs::create_dir_all(proj.join("memory")).unwrap();
        let transcript = proj.join("s.jsonl");
        let strip = |s: String| {
            let mut out = String::new();
            let mut in_esc = false;
            for c in s.chars() {
                match (in_esc, c) {
                    (false, '\u{1b}') => in_esc = true,
                    (true, 'm') => in_esc = false,
                    (false, c) => out.push(c),
                    _ => {}
                }
            }
            out
        };
        let render = || {
            let v = serde_json::json!({
                "model": {"display_name": "Opus 5"},
                "transcript_path": transcript.to_string_lossy(),
            });
            strip(header(&v, None).unwrap())
        };

        std::fs::write(proj.join("memory/MEMORY.md"), "a\nb\n").unwrap();
        let only_mem = render();
        assert!(only_mem.contains("mem "), "{only_mem}");
        assert!(
            !only_mem.contains("int "),
            "a project without intuition.md must render no int gauge: {only_mem}"
        );

        std::fs::write(proj.join("memory/intuition.md"), "x\ny\nz\n").unwrap();
        let both = render();
        let m = both.find("mem ").expect("mem gauge missing");
        let i = both.find("int ").expect("int gauge missing");
        assert!(m < i, "mem must precede int, got: {both}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Pay-as-you-go spend maths for API-key sessions.
//!
//! With API-key billing Claude Code sends no `rate_limits` — there is no account allowance
//! to show a percentage of — but `cost.total_cost_usd` is the session's actual
//! pay-as-you-go cost (subscription sessions carry only a shadow estimate). These helpers
//! rebuild a machine-wide "spent today" figure from the per-session cumulative counters in
//! the sample history: each session contributes its counter's movement across today's
//! observations (its last pre-midnight counter is the exact baseline when it spans
//! midnight).
//!
//! Attribution is deliberately conservative: spend that can't be *proven* to belong to
//! today — a session's counter before its first observation, or anything resting on the
//! payload's `total_duration_ms` (accumulated active time, back-dated by `--resume`, so
//! never a session age) — is excluded. The day figure undercounts rather than guesses,
//! and only explicitly flagged API samples count at all (see `history::is_api_sample`).

use std::collections::HashMap;

use crate::fmt::{fmt_dur, fmt_usd_rate, DIM, RED, RESET};
use crate::history::{is_api_sample, session_key, subscription_sids, Sample};

/// The live session's counter from the current stdin payload — fresher than the throttled
/// history, and present even before the session's first sample lands.
pub struct Live<'a> {
    pub sid: Option<&'a str>,
    pub usd: f64,
}

pub struct DaySpend {
    /// Dollars attributed to today across sessions.
    pub total: f64,
    /// Distinct sessions that contributed today.
    pub sessions: usize,
}

/// The fitted burn rate plus the portion of it that is well-enough evidenced to predict
/// from. The budget warning must not extrapolate a seconds-long spike, but the display
/// figure should still show it.
#[derive(Clone, Copy)]
pub struct DayRate {
    /// Total observed burn across sessions — the display figure.
    pub per_hr: f64,
    /// Burn from sessions whose own fit spans at least [`MIN_WARN_SPAN_SECS`] — what the
    /// budget ETA may be computed from. A per-session criterion: summing first and gating
    /// on the best single span would let one long-running session vouch for another's
    /// 60-second spike.
    pub warn_per_hr: f64,
}

/// A session's $ baseline at midnight: its last pre-midnight counter when it spans
/// midnight, else its first observation today. There is no "born today, count fully"
/// shortcut — the payload carries no trustworthy session age — so a session's pre-first-
/// observation spend is dropped (bounded by roughly one throttle interval of burn).
fn baseline(pre: &HashMap<String, f64>, k: &str, first_usd: f64) -> f64 {
    match pre.get(k) {
        Some(&b) => b.min(first_usd),
        None => first_usd,
    }
}

/// Today's spend across sessions from the API-key samples at/after `midnight`, with the
/// live session's current counter merged in. Samples without a session id are skipped —
/// they cannot be attributed to a baseline (and would otherwise collapse into one bucket).
pub fn day_spend(hist: &[Sample], midnight: f64, live: Option<&Live>) -> DaySpend {
    let sub_sids = subscription_sids(hist);

    // Last pre-midnight counter per session — the exact baseline for sessions spanning it.
    // Per-session max, not last-write-wins: cumulative counters are monotone, so max ==
    // latest, and it stays correct if a clock step ever leaves the file out of time order.
    let mut pre: HashMap<String, f64> = HashMap::new();
    for s in hist
        .iter()
        .filter(|s| is_api_sample(s) && (s.t as f64) < midnight)
    {
        if let (Some(u), Some(k)) = (
            s.usd.filter(|u| u.is_finite()),
            session_key(s.sid.as_deref()),
        ) {
            pre.entry(k).and_modify(|b| *b = b.max(u)).or_insert(u);
        }
    }

    // Per-session (baseline, latest) over today's samples.
    let mut state: HashMap<String, (f64, f64)> = HashMap::new();
    for s in hist
        .iter()
        .filter(|s| is_api_sample(s) && s.t as f64 >= midnight)
    {
        let (u, k) = match (
            s.usd.filter(|u| u.is_finite()),
            session_key(s.sid.as_deref()),
        ) {
            (Some(u), Some(k)) => (u, k),
            _ => continue,
        };
        if sub_sids.contains(&k) {
            continue;
        }
        state
            .entry(k.clone())
            .and_modify(|e| e.1 = e.1.max(u))
            .or_insert_with(|| (baseline(&pre, &k, u), u));
    }

    if let Some(lv) = live {
        if lv.usd.is_finite() {
            if let Some(k) = session_key(lv.sid) {
                if !sub_sids.contains(&k) {
                    state
                        .entry(k.clone())
                        .and_modify(|e| e.1 = e.1.max(lv.usd))
                        .or_insert_with(|| (baseline(&pre, &k, lv.usd), lv.usd));
                }
            }
        }
    }

    // An empty sum is -0.0 (f64's additive identity), which would render as `$-0`.
    let total: f64 = state.values().map(|(b, l)| (l - b).max(0.0)).sum::<f64>() + 0.0;
    DaySpend {
        total,
        sessions: state.len(),
    }
}

/// Minimum time base for a per-session fit — a shorter span divides a real dollar delta by
/// seconds and reads as an absurd $/h spike (e.g. right after midnight, or a live point
/// landing moments after a logged sample).
const MIN_FIT_SPAN_SECS: f64 = 60.0;

/// A session's fit must span this long before the *predictive* budget warning may
/// extrapolate from it. Claude Code spend arrives in per-request lumps ≥ a throttle
/// interval apart, so a single expensive request over a few minutes reads as an absurd
/// hourly rate; percentages in sub mode move smoothly, dollars don't.
pub const MIN_WARN_SPAN_SECS: f64 = 900.0;

/// $/hr across sessions over the trailing `window_sec`: each session's own samples (plus
/// the live point) are fitted independently and the slopes summed. Fitting per session —
/// rather than one machine-wide cumulative series — keeps a session's *accrued* spend from
/// entering the fit as an instantaneous step when it first appears mid-window, which would
/// otherwise read as a huge current burn. Cumulative counters make each per-session slope
/// immune to day boundaries and baselines.
///
/// Every non-live session also gets a synthetic flat point at `now`: its counter cannot
/// have decreased, and without the flat tail a session that exited after a burst would
/// keep reporting that burst as *current* burn until its samples aged out of the window.
/// If such a session is in fact alive but unsampled, the flat tail undercounts — the
/// declared bias.
pub fn day_rate(
    hist: &[Sample],
    live: Option<&Live>,
    window_sec: f64,
    now: f64,
) -> Option<DayRate> {
    let sub_sids = subscription_sids(hist);
    let live_key = live.and_then(|lv| session_key(lv.sid));

    let mut by_sid: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    for s in hist.iter().filter(|s| is_api_sample(s)) {
        if now - (s.t as f64) > window_sec {
            continue;
        }
        if let (Some(u), Some(k)) = (
            s.usd.filter(|u| u.is_finite()),
            session_key(s.sid.as_deref()),
        ) {
            if !sub_sids.contains(&k) {
                by_sid.entry(k).or_default().push((s.t as f64, u));
            }
        }
    }
    if let Some(lv) = live {
        if lv.usd.is_finite() {
            if let Some(k) = live_key.clone() {
                if !sub_sids.contains(&k) {
                    by_sid.entry(k).or_default().push((now, lv.usd));
                }
            }
        }
    }

    let mut total = 0.0;
    let mut warn_total = 0.0;
    let mut any = false;
    for (k, points) in by_sid.iter_mut() {
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        if live_key.as_deref() != Some(k.as_str()) {
            if let Some(&(t_last, u_last)) = points.last() {
                if t_last < now {
                    points.push((now, u_last)); // flat tail: burn decays once sampling stops
                }
            }
        }
        let span =
            points.last().map(|p| p.0).unwrap_or(0.0) - points.first().map(|p| p.0).unwrap_or(0.0);
        if points.len() < 2 || span < MIN_FIT_SPAN_SECS {
            continue;
        }
        if let Some(r) = crate::burn::slope_per_hr(points) {
            let r = r.max(0.0); // cumulative counters can't burn negatively
            total += r;
            if span >= MIN_WARN_SPAN_SECS {
                warn_total += r;
            }
            any = true;
        }
    }
    if any {
        Some(DayRate {
            per_hr: total,
            warn_per_hr: warn_total,
        })
    } else {
        None
    }
}

/// `QUOTALINE_DAILY_BUDGET` (USD, a plain number) — opts the day line into a bar with % of
/// budget and a budget-ETA warning. Shared by the status line and the report so the two
/// can never disagree about the budget.
pub fn daily_budget() -> Option<f64> {
    daily_budget_from(std::env::var("QUOTALINE_DAILY_BUDGET").ok()?)
}

/// Whether `QUOTALINE_DAILY_BUDGET` is set at all (even to something unparseable) — lets
/// the report tell the user their value was rejected, where the status line must stay
/// silent.
pub fn budget_var_set() -> bool {
    std::env::var_os("QUOTALINE_DAILY_BUDGET").is_some_and(|v| !v.is_empty())
}

fn daily_budget_from(v: String) -> Option<f64> {
    let f: f64 = v.trim().parse().ok()?;
    (f.is_finite() && f > 0.0).then_some(f)
}

/// Burn below this reads as idle on both surfaces (the report prints `~idle`, the status
/// line suppresses the `↑` readout) — one threshold so the two can't disagree.
pub const IDLE_RATE_PER_HR: f64 = 0.005;

/// How today's spend stands against the daily budget — the single decision both renderers
/// format in their own style.
pub enum BudgetOutcome {
    /// Already at/over the budget (needs no rate to know).
    Over,
    /// At the well-evidenced portion of the current rate, the budget lands in `eta_secs`,
    /// before the midnight rollover.
    HitsBudget { eta_secs: f64 },
    /// The day rolls over before the budget would be hit (or the burn is negligible, or
    /// too thinly evidenced to predict from).
    RollsOverFirst,
}

pub fn budget_outcome(
    today: f64,
    budget: f64,
    rate: Option<DayRate>,
    next_midnight: f64,
    now: f64,
) -> BudgetOutcome {
    if today >= budget {
        return BudgetOutcome::Over;
    }
    if let Some(r) = rate.filter(|r| r.warn_per_hr > 0.01) {
        let eta = (budget - today) / r.warn_per_hr * 3600.0;
        if eta > 0.0 && eta < next_midnight - now {
            return BudgetOutcome::HitsBudget { eta_secs: eta };
        }
    }
    BudgetOutcome::RollsOverFirst
}

/// Inline burn readout for the day line. Returns `(plain, coloured)`; `plain` is used only
/// for width measurement. The over-budget flag needs no rate, so it renders even when the
/// history is too thin to fit one (fresh install, first minutes of a day).
pub fn spend_suffix(
    today: f64,
    budget: Option<f64>,
    rate: Option<DayRate>,
    next_midnight: f64,
    now: f64,
) -> (String, String) {
    let warn = budget.and_then(
        |b| match budget_outcome(today, b, rate, next_midnight, now) {
            BudgetOutcome::Over => Some("⚠ over budget".to_string()),
            BudgetOutcome::HitsBudget { eta_secs } => {
                Some(format!("⚠ budget {}", fmt_dur(eta_secs as i64)))
            }
            BudgetOutcome::RollsOverFirst => None,
        },
    );
    let body = rate
        .filter(|r| r.per_hr > IDLE_RATE_PER_HR)
        .map(|r| format!("↑{}/h", fmt_usd_rate(r.per_hr)));
    if body.is_none() && warn.is_none() {
        return (String::new(), String::new());
    }

    let mut plain = String::new();
    let mut colored = String::new();
    if let Some(b) = &body {
        plain.push_str(b);
        colored.push_str(&format!("{DIM}{b}{RESET}"));
    }
    if let Some(w) = &warn {
        if !plain.is_empty() {
            plain.push_str("  ");
            colored.push_str("  ");
        }
        plain.push_str(w);
        colored.push_str(&format!("{RED}{w}{RESET}"));
    }
    (plain, colored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_sample(t: i64, sid: &str, usd: f64) -> Sample {
        Sample {
            t,
            sid: Some(sid.to_string()),
            usd: Some(usd),
            api: true,
            ..Default::default()
        }
    }

    fn sub_sample(t: i64, sid: &str, usd: f64) -> Sample {
        Sample {
            t,
            sid: Some(sid.to_string()),
            usd: Some(usd),
            h5: Some(10.0),
            d7: Some(20.0),
            ..Default::default()
        }
    }

    /// A rate whose whole burn is well-enough evidenced for the budget warning.
    fn rated(per_hr: f64) -> Option<DayRate> {
        Some(DayRate {
            per_hr,
            warn_per_hr: per_hr,
        })
    }

    const MID: f64 = 1000.0;

    #[test]
    fn only_observed_movement_counts() {
        // A session's counter before its first observation is unknowable spend — even for
        // a session that (per any payload hint) "started today". No fabrication.
        let hist = vec![api_sample(2000, "a", 3.0), api_sample(3000, "a", 5.0)];
        let d = day_spend(&hist, MID, None);
        assert_eq!(d.sessions, 1);
        assert!((d.total - 2.0).abs() < 1e-9, "got {}", d.total);
    }

    #[test]
    fn session_spanning_midnight_uses_pre_midnight_baseline() {
        let hist = vec![
            api_sample(900, "a", 10.0), // before midnight: baseline $10
            api_sample(2000, "a", 14.0),
        ];
        let d = day_spend(&hist, MID, None);
        assert!((d.total - 4.0).abs() < 1e-9, "got {}", d.total);
    }

    #[test]
    fn sessions_sum_and_sub_samples_ignored() {
        let hist = vec![
            api_sample(2000, "a", 3.0),
            api_sample(2600, "a", 6.0),
            sub_sample(2500, "sub", 99.0), // subscription shadow cost must not count
            api_sample(3000, "b", 2.0),
            api_sample(3050, "b", 2.5),
        ];
        let d = day_spend(&hist, MID, None);
        assert_eq!(d.sessions, 2);
        assert!((d.total - 3.5).abs() < 1e-9, "got {}", d.total);
    }

    #[test]
    fn unflagged_samples_never_count_as_spend() {
        // A sample with cost and no windows but no explicit flag — a stripped API sample or
        // a degenerate subscription sample; either way it must not count as dollars.
        let stripped = Sample {
            t: 2000,
            sid: Some("stripped-sid".to_string()),
            usd: Some(12.4),
            ..Default::default()
        };
        let mut later = stripped.clone();
        later.t = 2600;
        later.usd = Some(13.4);
        // Even alongside a genuinely flagged API session in the same history.
        let flagged_a = api_sample(2900, "realapi", 1.0);
        let flagged_b = api_sample(2960, "realapi", 1.5);
        let d = day_spend(&[stripped, later, flagged_a, flagged_b], MID, None);
        assert_eq!(d.sessions, 1);
        assert!((d.total - 0.5).abs() < 1e-9, "got {}", d.total);
    }

    #[test]
    fn mislabelled_subscription_session_is_excluded_retroactively() {
        // A session that ever logged a subscription sample is a subscription session; an
        // api-flagged sample it left behind (resumed-session transient) must not count.
        let hist = vec![
            api_sample(2000, "resumed", 12.5), // mislabelled transient
            sub_sample(2500, "resumed", 13.0), // truth arrives
            api_sample(3000, "realapi", 2.0),
            api_sample(3060, "realapi", 2.5),
        ];
        let d = day_spend(&hist, MID, None);
        assert_eq!(d.sessions, 1);
        assert!((d.total - 0.5).abs() < 1e-9, "got {}", d.total);

        // The live payload of a known-subscription session is excluded the same way.
        let live = Live {
            sid: Some("resumed"),
            usd: 20.0,
        };
        let d2 = day_spend(&hist, MID, Some(&live));
        assert_eq!(d2.sessions, 1);
        assert!((d2.total - 0.5).abs() < 1e-9, "got {}", d2.total);
    }

    #[test]
    fn pre_midnight_baseline_is_order_independent() {
        // Two pre-midnight samples out of time order (clock step): the baseline must be the
        // counter maximum, not whichever the file happens to list last.
        let hist = vec![
            api_sample(950, "a", 10.0), // later sample listed first
            api_sample(900, "a", 8.0),
            api_sample(2000, "a", 12.0),
        ];
        let d = day_spend(&hist, MID, None);
        assert!((d.total - 2.0).abs() < 1e-9, "got {}", d.total); // 12 − max(10, 8)
    }

    #[test]
    fn sid_less_samples_are_skipped_not_merged() {
        let mut a = api_sample(2000, "x", 5.0);
        a.sid = None;
        let mut b = api_sample(3000, "y", 3.0);
        b.sid = None;
        let d = day_spend(&[a, b], MID, None);
        assert_eq!(d.sessions, 0);
        assert_eq!(
            d.total.to_bits(),
            0.0f64.to_bits(),
            "positive zero, not -0.0"
        );
    }

    #[test]
    fn live_point_extends_sessions() {
        // Live counter is fresher than the last sample of the same session.
        let hist = vec![
            api_sample(900, "abcdefghijkl-full-sid", 1.0), // pre-midnight baseline $1
            api_sample(2000, "abcdefghijkl-full-sid", 3.0),
        ];
        let live = Live {
            sid: Some("abcdefghijkl-full-sid"), // must match via 12-char truncation
            usd: 4.5,
        };
        let d = day_spend(&hist, MID, Some(&live));
        assert_eq!(d.sessions, 1);
        assert!((d.total - 3.5).abs() < 1e-9, "got {}", d.total);

        // A live session with no samples yet contributes presence, not unprovable dollars.
        let live2 = Live {
            sid: Some("fresh"),
            usd: 1.25,
        };
        let d2 = day_spend(&[], MID, Some(&live2));
        assert_eq!(d2.sessions, 1);
        assert!((d2.total - 0.0).abs() < 1e-9, "got {}", d2.total);
    }

    #[test]
    fn day_rate_fits_per_session() {
        // $2/h and $1/h sessions sum to $3/h; the live point marks "a" as still alive.
        let hist = vec![
            api_sample(0, "a", 0.0),
            api_sample(1800, "a", 1.0),
            api_sample(3600, "a", 2.0),
            api_sample(0, "b", 5.0),
            api_sample(3600, "b", 6.0),
        ];
        let live = Live {
            sid: Some("a"),
            usd: 2.0,
        };
        let r = day_rate(&hist, Some(&live), 7200.0, 3600.0).unwrap();
        assert!((r.per_hr - 3.0).abs() < 1e-9, "got {}", r.per_hr);
        // Both sessions span an hour, so the whole rate is warnable.
        assert!((r.warn_per_hr - 3.0).abs() < 1e-9, "got {}", r.warn_per_hr);
    }

    #[test]
    fn day_rate_ignores_accrued_step_from_newly_visible_session() {
        // Session "old" appears mid-window with $8 accrued over hours: a machine-wide
        // cumulative fit would read that as a step of current burn. Per-session fitting
        // sees only its (tiny) in-window movement.
        let hist = vec![
            api_sample(0, "a", 0.0),
            api_sample(3600, "a", 1.0),
            api_sample(3000, "old", 8.0),
            api_sample(3600, "old", 8.1),
        ];
        let r = day_rate(&hist, None, 7200.0, 3600.0).unwrap();
        // a: $1/h; old: 0.1 over 600s = $0.6/h. Nothing like the $8 step.
        assert!((r.per_hr - 1.6).abs() < 1e-6, "got {}", r.per_hr);
    }

    #[test]
    fn day_rate_decays_for_exited_sessions() {
        // A session bursts $3 in 5 minutes then exits; 90 minutes later its burst must not
        // still read as current burn. The flat tail at `now` dilutes the slope.
        let hist = vec![api_sample(0, "gone", 1.0), api_sample(300, "gone", 4.0)];
        let r = day_rate(&hist, None, 7200.0, 5700.0).unwrap();
        // Least squares over (0,1),(300,4),(5700,4): far below the burst's $36/h.
        assert!(r.per_hr < 2.0, "got {}", r.per_hr);
    }

    #[test]
    fn day_rate_rejects_ill_conditioned_fits() {
        // Live session: no flat tail, so two points seconds apart must not produce an
        // absurd spike.
        let hist = vec![api_sample(3600, "a", 0.02)];
        let live = Live {
            sid: Some("a"),
            usd: 0.10,
        };
        assert!(day_rate(&hist, Some(&live), 7200.0, 3605.0).is_none());
        // A single point with a flat tail fits a zero slope rather than nothing.
        let r = day_rate(&hist, None, 7200.0, 3700.0).unwrap();
        assert_eq!(r.per_hr, 0.0);
        assert!(day_rate(&[], None, 7200.0, 3700.0).is_none());
    }

    #[test]
    fn warnable_rate_is_gated_per_session_not_by_the_best_span() {
        // Session A: steady $0.5/h over 2h. Session B (live): $120/h over a 60s base.
        // A's long span must not vouch for B's spike — only A's burn is warnable.
        let hist = vec![
            api_sample(0, "steady", 0.0),
            api_sample(3600, "steady", 0.5),
            api_sample(7200, "steady", 1.0),
            api_sample(7140, "spiky", 1.0),
        ];
        let live = Live {
            sid: Some("spiky"),
            usd: 3.0,
        };
        let r = day_rate(&hist, Some(&live), 7200.0, 7200.0).unwrap();
        assert!(r.per_hr > 100.0, "display shows the spike: {}", r.per_hr);
        assert!(
            r.warn_per_hr < 1.0,
            "prediction excludes it: {}",
            r.warn_per_hr
        );

        // Consequently no budget warning fires from the spike alone…
        let (plain, _) = spend_suffix(4.0, Some(50.0), Some(r), 80_000.0, 7200.0);
        assert!(plain.contains("↑") && !plain.contains("⚠"), "{plain}");
        // …until the spiky session accumulates its own evidence span.
        let (plain2, _) = spend_suffix(4.0, Some(50.0), rated(120.0), 80_000.0, 7200.0);
        assert!(plain2.contains("⚠ budget"), "{plain2}");
    }

    #[test]
    fn suffix_warns_only_when_budget_hit_before_midnight() {
        // $10 spent of $20, burning $5/h, midnight 10h away → hits budget in 2h → warn.
        let (plain, _) = spend_suffix(10.0, Some(20.0), rated(5.0), 36_000.0, 0.0);
        assert!(plain.contains("budget"), "{plain}");
        // Midnight in 1h → rolls over before the budget is hit → no warning.
        let (plain2, _) = spend_suffix(10.0, Some(20.0), rated(5.0), 3600.0, 0.0);
        assert!(!plain2.contains("budget"), "{plain2}");
        // Already over → explicit flag regardless of rate.
        let (plain3, _) = spend_suffix(25.0, Some(20.0), rated(0.0), 36_000.0, 0.0);
        assert!(plain3.contains("over budget"), "{plain3}");
        // Over budget with NO rate at all (fresh install): the flag must still show.
        let (plain4, _) = spend_suffix(25.0, Some(20.0), None, 36_000.0, 0.0);
        assert!(plain4.contains("over budget"), "{plain4}");
        // No budget → rate only, no warnings.
        let (plain5, _) = spend_suffix(10.0, None, rated(5.0), 36_000.0, 0.0);
        assert!(plain5.contains("↑") && !plain5.contains("⚠"), "{plain5}");
        // No rate, no budget trouble → empty.
        assert_eq!(spend_suffix(10.0, None, None, 36_000.0, 0.0).0, "");
        assert_eq!(spend_suffix(10.0, Some(20.0), None, 36_000.0, 0.0).0, "");
    }

    #[test]
    fn idle_rate_is_suppressed_inline() {
        // Below the shared idle threshold the ↑ readout disappears (the report prints
        // ~idle for the same input).
        let idle = Some(DayRate {
            per_hr: 0.004,
            warn_per_hr: 0.004,
        });
        assert_eq!(spend_suffix(1.0, None, idle, 36_000.0, 0.0).0, "");
    }

    #[test]
    fn budget_parse() {
        assert_eq!(daily_budget_from("50".into()), Some(50.0));
        assert_eq!(daily_budget_from(" 12.5 ".into()), Some(12.5));
        assert_eq!(daily_budget_from("$50".into()), None);
        assert_eq!(daily_budget_from("0".into()), None);
        assert_eq!(daily_budget_from("-5".into()), None);
        assert_eq!(daily_budget_from("inf".into()), None);
        assert_eq!(daily_budget_from("nan".into()), None);
        assert_eq!(daily_budget_from("abc".into()), None);
    }
}

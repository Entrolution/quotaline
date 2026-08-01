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
use crate::history::{is_api_sample, session_key, Sample, SubSessions};

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

/// Today's spend across sessions from the API-key samples at/after `midnight`, with the
/// live session's current counter merged in. Samples without a session id are skipped —
/// they cannot be attributed to a baseline (and would otherwise collapse into one bucket).
///
/// A flagged sample is dropped when its session was reporting subscription windows around
/// the time it was taken: the flag is written from the payload's shape, so the resume
/// transient can flag one stray sample before the truth arrives. Judging each sample by its
/// own moment — rather than excluding every session that ever held a subscription — is what
/// lets a session that genuinely switched auth mid-life contribute the dollars it spent
/// after the switch.
///
/// That per-moment judgement is also why a session's observations are differenced in
/// *runs*. `cost.total_cost_usd` is one counter that keeps climbing through a subscription
/// phase on the shadow estimate, so a session that ran on an API key, moved to a
/// subscription, and came back would otherwise have the whole shadow climb differenced into
/// its day total. A run ends wherever the session reported window data between two
/// observations, and the next one re-bases: the climb across the phase is discarded rather
/// than guessed at, the same direction every other unknown here is resolved.
pub fn day_spend(hist: &[Sample], midnight: f64, live: Option<&Live>, now: f64) -> DaySpend {
    let subs = SubSessions::build(hist);

    // Last pre-midnight counter per session — the exact baseline for sessions spanning it.
    // Per-session max, not last-write-wins: cumulative counters are monotone, so max ==
    // latest, and it stays correct if a clock step ever leaves the file out of time order.
    // Its time is kept too, because a baseline is only usable if no subscription phase sits
    // between it and today's first observation.
    let mut pre: HashMap<String, (i64, f64)> = HashMap::new();
    for s in hist
        .iter()
        .filter(|s| is_api_sample(s) && (s.t as f64) < midnight)
    {
        if let (Some(u), Some(k)) = (
            s.usd.filter(|u| u.is_finite()),
            session_key(s.sid.as_deref()),
        ) {
            // A `pre` entry can only *lower* a session's baseline, so it can only raise
            // what that session is charged — a stray sample is filtered like the rest.
            if subs.active_at(&k, s.t) {
                continue;
            }
            pre.entry(k)
                .and_modify(|e| {
                    // Ties go to the later sample: equal counters are equally good
                    // baselines, but the earlier one spans more history and is the more
                    // likely to be rejected by the subscription-phase guard below.
                    if u > e.1 || (u == e.1 && s.t > e.0) {
                        *e = (s.t, u);
                    }
                })
                .or_insert((s.t, u));
        }
    }

    // Today's attributable observations per session, in time order.
    let mut obs: HashMap<String, Vec<(i64, f64)>> = HashMap::new();
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
        if subs.active_at(&k, s.t) {
            continue;
        }
        obs.entry(k).or_default().push((s.t, u));
    }

    if let Some(lv) = live {
        if lv.usd.is_finite() {
            if let Some(k) = session_key(lv.sid) {
                if !subs.active_at(&k, now as i64) {
                    obs.entry(k).or_default().push((now as i64, lv.usd));
                }
            }
        }
    }

    let mut total = 0.0;
    for (k, points) in obs.iter_mut() {
        points.sort_by_key(|p| p.0);
        let (first_t, first_u) = points[0];
        // The pre-midnight counter is the exact baseline only if the session stayed on the
        // API key across midnight; a subscription phase in between makes the climb since
        // then shadow cost, so today starts from its own first observation. There is no
        // "born today, count fully" shortcut either — the payload carries no trustworthy
        // session age — so spend before that first observation is dropped: about one
        // throttle interval for a session that simply started, but up to ~45 min of burn
        // for one re-basing after a phase, since nothing it logs is attributable until the
        // hold has been served and the shadow band has passed.
        let mut base = match pre.get(k) {
            Some(&(pt, pu)) if !subs.windowed_between(k, pt, first_t) => pu.min(first_u),
            _ => first_u,
        };
        let mut last = first_u;
        let mut prev_t = first_t;
        for &(t, u) in points.iter().skip(1) {
            if subs.windowed_between(k, prev_t, t) {
                total += (last - base).max(0.0); // close the run before the phase
                base = u; // and re-base after it
                last = u;
            } else {
                // Max, not the last value listed. A cumulative counter cannot fall, so a
                // lower later reading means the file is out of time order (a clock step),
                // never a refund — taking the max keeps real spend from being dropped.
                last = last.max(u);
            }
            prev_t = t;
        }
        total += (last - base).max(0.0);
    }

    DaySpend {
        // An empty sum is -0.0 (f64's additive identity), which would render as `$-0`.
        total: total + 0.0,
        sessions: obs.len(),
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
    let subs = SubSessions::build(hist);
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
            // Only the session's current run, for the reason `day_spend` documents: a point
            // from before its last subscription phase would drag the shadow climb across
            // that phase into the slope, reading as a burst of current burn.
            if subs
                .last_window_at(&k, now as i64)
                .is_some_and(|w| s.t <= w)
            {
                continue;
            }
            if !subs.active_at(&k, s.t) {
                by_sid.entry(k).or_default().push((s.t as f64, u));
            }
        }
    }
    if let Some(lv) = live {
        if lv.usd.is_finite() {
            if let Some(k) = live_key.clone() {
                if !subs.active_at(&k, now as i64) {
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
    /// Just past the last sample in these fixtures — the live point's own moment, which
    /// decides whether a live session is still reporting subscription windows.
    const NOW: f64 = 3100.0;

    #[test]
    fn only_observed_movement_counts() {
        // A session's counter before its first observation is unknowable spend — even for
        // a session that (per any payload hint) "started today". No fabrication.
        let hist = vec![api_sample(2000, "a", 3.0), api_sample(3000, "a", 5.0)];
        let d = day_spend(&hist, MID, None, NOW);
        assert_eq!(d.sessions, 1);
        assert!((d.total - 2.0).abs() < 1e-9, "got {}", d.total);
    }

    #[test]
    fn session_spanning_midnight_uses_pre_midnight_baseline() {
        let hist = vec![
            api_sample(900, "a", 10.0), // before midnight: baseline $10
            api_sample(2000, "a", 14.0),
        ];
        let d = day_spend(&hist, MID, None, NOW);
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
        let d = day_spend(&hist, MID, None, NOW);
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
        let d = day_spend(&[stripped, later, flagged_a, flagged_b], MID, None, NOW);
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
        let d = day_spend(&hist, MID, None, NOW);
        assert_eq!(d.sessions, 1);
        assert!((d.total - 0.5).abs() < 1e-9, "got {}", d.total);

        // The live payload of a known-subscription session is excluded the same way.
        let live = Live {
            sid: Some("resumed"),
            usd: 20.0,
        };
        let d2 = day_spend(&hist, MID, Some(&live), NOW);
        assert_eq!(d2.sessions, 1);
        assert!((d2.total - 0.5).abs() < 1e-9, "got {}", d2.total);
    }

    #[test]
    fn session_that_switched_auth_contributes_its_post_switch_spend() {
        // The bug this guards: a session that reported windows for hours and then billed
        // to an API key used to be excluded for the rest of its life, so its real dollars
        // never reached the day figure. Only the samples taken beside the windows are
        // shadow cost; the ones from well after the switch are money.
        let hold = crate::history::SUB_HOLD_SECS;
        let hist = vec![
            sub_sample(1200, "sw", 20.0),
            sub_sample(1500, "sw", 28.0),
            api_sample(1560, "sw", 28.0), // stray flag beside the windows: not spend
            api_sample(1500 + hold + 60, "sw", 30.0),
            api_sample(1500 + hold + 660, "sw", 33.0),
        ];
        let d = day_spend(&hist, MID, None, (1500 + hold + 700) as f64);
        assert_eq!(d.sessions, 1);
        assert!((d.total - 3.0).abs() < 1e-9, "got {}", d.total);
    }

    #[test]
    fn a_counter_is_never_differenced_across_a_subscription_phase() {
        // API-key leg, then the session is resumed without the key and spends hours on the
        // subscription — where the same counter keeps climbing on the *shadow* estimate —
        // then switches back. Differencing first-to-last would bank that whole climb as
        // money. Only the two API-key legs' own movement is real.
        let hold = crate::history::SUB_HOLD_SECS;
        let mut hist = vec![api_sample(1200, "s", 5.0), api_sample(1380, "s", 5.6)];
        hist.extend((0..40).map(|i| sub_sample(4100 + i * 60, "s", 12.0 + i as f64 * 2.0)));
        let last_win = 4100 + 39 * 60;
        hist.push(api_sample(last_win + hold + 60, "s", 92.0));
        let d = day_spend(&hist, MID, None, (last_win + hold + 100) as f64);
        assert_eq!(d.sessions, 1);
        // 5.6 − 5.0 from the first leg; the second leg has one observation, so no movement.
        assert!((d.total - 0.6).abs() < 1e-9, "got {}", d.total);

        // Same shape across midnight: the pre-midnight counter is not a valid baseline for
        // today once a subscription phase sits between the two.
        let mut hist2 = vec![api_sample(-3600, "s", 5.0)];
        hist2.extend((0..40).map(|i| sub_sample(1000 + i * 60, "s", 5.0 + i as f64 * 2.0)));
        let last_win2 = 1000 + 39 * 60;
        hist2.push(api_sample(last_win2 + hold + 60, "s", 92.0));
        hist2.push(api_sample(last_win2 + hold + 660, "s", 94.0));
        let d2 = day_spend(&hist2, MID, None, (last_win2 + hold + 700) as f64);
        assert!((d2.total - 2.0).abs() < 1e-9, "got {}", d2.total);
    }

    #[test]
    fn a_stray_pre_midnight_flag_cannot_become_a_baseline() {
        // The resume transient can flag one sample beside the windows. Landing before
        // midnight it would become the baseline — and a baseline only ever *lowers* what is
        // subtracted, so a stray one manufactures dollars rather than losing them.
        let hold = crate::history::SUB_HOLD_SECS;
        let hist = vec![
            sub_sample(800, "sw", 50.0),
            api_sample(850, "sw", 3.0), // stray, beside the windows
            api_sample(800 + hold + 60, "sw", 30.0),
            api_sample(800 + hold + 660, "sw", 33.0),
        ];
        let d = day_spend(&hist, MID, None, (800 + hold + 700) as f64);
        assert_eq!(d.sessions, 1);
        assert!((d.total - 3.0).abs() < 1e-9, "got {}", d.total);
    }

    #[test]
    fn day_rate_fits_only_the_current_run() {
        // A point from before the session's subscription phase would drag the shadow climb
        // across that phase into the slope and read as a burst of current burn.
        let hist = vec![
            api_sample(0, "s", 1.0),
            sub_sample(2760, "s", 1.1),
            sub_sample(4000, "s", 25.0),
            api_sample(6820, "s", 25.2),
        ];
        let live = Live {
            sid: Some("s"),
            usd: 25.4,
        };
        // The current run really is burning ($0.20 over 600s = $1.2/h) and that is what
        // shows — not the ~$12/h the pre-phase point would manufacture.
        let r = day_rate(&hist, Some(&live), 7200.0, 7420.0).unwrap();
        assert!((r.per_hr - 1.2).abs() < 0.05, "got {}", r.per_hr);

        // A stray flag *after* the last window is caught by the other filter: it sits
        // beside the windows, where the counter is still the shadow estimate.
        let mut with_stray = hist.clone();
        with_stray.push(api_sample(4100, "s", 1.2));
        let r2 = day_rate(&with_stray, Some(&live), 7200.0, 7420.0).unwrap();
        assert!((r2.per_hr - 1.2).abs() < 0.05, "got {}", r2.per_hr);
    }

    #[test]
    fn a_run_is_differenced_on_its_maximum_not_its_last_listed_sample() {
        // Same invariant the pre-midnight baseline has: a clock step can leave today's
        // samples out of time order, and a counter that appears to dip must not shrink what
        // the session is charged.
        let hist = vec![
            api_sample(2000, "a", 5.0),
            api_sample(2600, "a", 9.0),
            api_sample(3000, "a", 7.0),
        ];
        let d = day_spend(&hist, MID, None, NOW);
        assert!((d.total - 4.0).abs() < 1e-9, "got {}", d.total);
    }

    #[test]
    fn every_subscription_phase_re_bases_not_just_the_first() {
        // API → sub → API → sub → API. Each phase closes a run and the next re-bases, so
        // only the three API legs' own movement is money.
        let hold = crate::history::SUB_HOLD_SECS;
        let gap = hold + 60; // clear of the ±hold shadow on both sides
        let mut hist = vec![api_sample(1100, "s", 1.0), api_sample(1160, "s", 1.5)];
        let w1 = 1160 + gap;
        hist.push(sub_sample(w1, "s", 20.0)); // shadow climb through phase 1
        hist.push(api_sample(w1 + gap, "s", 40.0));
        hist.push(api_sample(w1 + gap + 60, "s", 40.5));
        let w2 = w1 + gap + 60 + gap;
        hist.push(sub_sample(w2, "s", 70.0)); // shadow climb through phase 2
        hist.push(api_sample(w2 + gap, "s", 90.0));
        hist.push(api_sample(w2 + gap + 60, "s", 90.25));
        let d = day_spend(&hist, MID, None, (w2 + gap + 100) as f64);
        // 0.5 + 0.5 + 0.25 — never the $89.25 a single difference would bank.
        assert!((d.total - 1.25).abs() < 1e-9, "got {}", d.total);
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
        let d = day_spend(&hist, MID, None, NOW);
        assert!((d.total - 2.0).abs() < 1e-9, "got {}", d.total); // 12 − max(10, 8)
    }

    #[test]
    fn sid_less_samples_are_skipped_not_merged() {
        let mut a = api_sample(2000, "x", 5.0);
        a.sid = None;
        let mut b = api_sample(3000, "y", 3.0);
        b.sid = None;
        let d = day_spend(&[a, b], MID, None, NOW);
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
        let d = day_spend(&hist, MID, Some(&live), NOW);
        assert_eq!(d.sessions, 1);
        assert!((d.total - 3.5).abs() < 1e-9, "got {}", d.total);

        // A live session with no samples yet contributes presence, not unprovable dollars.
        let live2 = Live {
            sid: Some("fresh"),
            usd: 1.25,
        };
        let d2 = day_spend(&[], MID, Some(&live2), NOW);
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

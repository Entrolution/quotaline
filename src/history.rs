//! The usage-history log, shared with `report` and the inline burn-rate readout.
//!
//! Best-effort, throttled (~1/min), pruned, and written via atomic replace so the file is
//! never half-written. It is lock-free: the atomic rename prevents corruption, and because
//! the throttle is checked against the shared file, concurrent double-writes are rare and a
//! dropped sample is harmless. The throttle is deliberately global (keyed on the last entry
//! regardless of session): concurrent sessions share the 1/min slot, which bounds write
//! volume; the day-spend aggregate tolerates the resulting per-session gaps because its
//! unknown-baseline path undercounts rather than guesses.
//!
//! For subscription samples the on-disk schema matches the original Python tool's, so an
//! existing `usage-history.json` carries over unchanged. API-key samples add the
//! quotaline-only `api` flag, elided when false, so old and new files stay mutually
//! readable.
//!
//! One asymmetry is deliberate. The window-less *probes* written while a session's switch
//! away from a subscription is unproven parse fine in any version, but a pre-probe binary's
//! `report` picks the newest non-API sample and finds no percentages on it, so its 5h/wk
//! sections stay blank while a probe is the trailing entry (its status line is unaffected).
//! No probe shape avoids this: the only samples that binary's `analyze` skips are exactly
//! the ones its `day_spend` counts as money, and flagging a subscription session's shadow
//! cost as spend is the one error that must never ship. Upgrading fixes it.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json::{f64_at, get_pct, get_reset, nested, payload_mode, Mode};

pub const SAMPLE_INTERVAL: i64 = 60; // seconds; min gap between logged samples (global)

// Subscription-only histories keep the original cap (~4h at the 1/min throttle — well past
// the 2h burn-rate windows). Once API-key samples exist the cap grows so the "spent today"
// aggregate can see a full local day (~25h ≈ 200 KB at ~140 B/sample); subscription-only
// users never pay that read/parse cost.
pub const MAX_ENTRIES: usize = 250;
pub const MAX_ENTRIES_API: usize = 1500;
pub const MAX_AGE_SECS: i64 = 26 * 3600; // drop samples older than any consumer's lookback
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
    /// API-key (pay-as-you-go) sample: `usd` is actual pay-as-you-go cost, and there are no
    /// window percentages. Skipped when false so subscription histories keep their old
    /// shape. An older binary rewriting the shared file strips this field; such samples
    /// simply stop counting as spend (see [`is_api_sample`]) until fresh flagged ones land.
    /// (A `dur` field once stored `cost.total_duration_ms` to detect sessions born today,
    /// but that value is *accumulated active* time — `--resume` back-dates it past idle
    /// gaps — so "born today" was unprovable and the field would fabricate spend. Unknown
    /// keys are ignored on read, so files that carried it still parse.)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub api: bool,
    /// The payload carried a `rate_limits` object but no window value could be read from
    /// it — a null or renamed inner field, or a shape this version doesn't know. Written
    /// only in that case, so an ordinary subscription sample keeps its legacy shape.
    ///
    /// Without it such a sample is stored byte-identical to a hold probe, and the two mean
    /// opposite things: the session is *provably* on a subscription, yet everything
    /// downstream would read "cost with no windows" and conclude the opposite — banking the
    /// phase's shadow climb as spend and satisfying the switch proof outright. Classifying
    /// from the parsed values alone discards a verdict [`payload_mode`] already reached.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sub: bool,
}

/// Whether a sample is known to have come from a subscription payload — window values read
/// off it, or [`Sample::sub`] recording that its `rate_limits` object didn't parse.
pub fn is_sub_sample(s: &Sample) -> bool {
    has_window_data(s) || s.sub
}

/// Whether a sample records API-key spend: the explicit flag only. A shape heuristic
/// ("carries cost, no windows") was considered for surviving an older binary's rewrite of
/// the shared file (which strips the flag), but a *subscription* sample whose windows
/// didn't parse has the identical shape, and counting its shadow cost as spend fabricates
/// dollars — the one direction the day figure must never err. Flag-only means a stripped
/// history undercounts until new flagged samples accumulate, which is the declared bias.
pub fn is_api_sample(s: &Sample) -> bool {
    s.api
}

/// Whether a sample carries any trace of a subscription window — the mark of a payload
/// that Claude Code served under Pro/Max auth.
pub fn has_window_data(s: &Sample) -> bool {
    s.h5.is_some() || s.d7.is_some() || s.h5r.is_some() || s.d7r.is_some()
}

/// A sample with cost that cannot be shown to have come from a subscription. Not proof of
/// API-key spend (see [`is_api_sample`]) — but exactly the right boundary where *exclusion*
/// is the safe direction: such a sample's dollars may be stripped API spend, so they must
/// not feed subscription-percentage maths (the report's $/1% anchor). A `sub`-flagged
/// sample is excepted because it is the one case that rationale doesn't cover: its dollars
/// are known to be a subscription's, so they belong in that anchor.
pub fn lacks_window_data(s: &Sample) -> bool {
    s.usd.is_some() && !is_sub_sample(s)
}

/// The session key as stored on disk: `session_id` truncated to 12 chars, `None` when the
/// payload carried none. Shared with the day-spend aggregation so live payloads and stored
/// samples always normalise identically.
pub fn session_key(sid: Option<&str>) -> Option<String> {
    let s: String = sid?.chars().take(12).collect();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// How long cost must be seen moving with no window data before a session that reported
/// windows is reclassified, and the margin either side of a windowed sample within which a
/// cost sample is treated as that session's shadow estimate. Generous on purpose: the cost
/// of waiting is a delayed mode switch, the cost of switching early is presenting a
/// subscriber's shadow cost as real spend.
pub const SUB_HOLD_SECS: i64 = 45 * 60;

/// How many quiet sessions may reserve samples against pruning at once (two each: the
/// window sample a hold rests on and the cost sample that marks the session quiet). Well
/// above the number of sessions a machine runs concurrently, and small against the smaller
/// retention cap, so the reservation can never crowd out the history it sits in.
pub const MAX_ANCHOR_SESSIONS: usize = 16;

#[derive(Default)]
struct SidTrace {
    /// Times of this session's samples that carried subscription window data.
    windows: Vec<i64>,
    /// `(time, counter)` of its window-less cost samples — the probes `log_sample` writes
    /// while a switch is unproven, plus any API-key samples it went on to log.
    costs: Vec<(i64, f64)>,
}

/// Per-session view of *when* a session looked like a subscription — the input to every
/// decision that must not mistake shadow cost for spend.
///
/// On current Claude Code, `claude --resume` restores a session's *cost* immediately but
/// repopulates `rate_limits` only from the first fresh API response, so a resumed Pro/Max
/// session briefly looks API-key-shaped. The same session id can also change billing for
/// real: `--resume` reuses the id, and the resumed process inherits whatever auth its shell
/// provides, so a session that spent eleven hours on a subscription can spend the rest of
/// its life on an API key. Classification is therefore a question about a *moment*, never a
/// flat set of session ids.
pub struct SubSessions(std::collections::HashMap<String, SidTrace>);

impl SubSessions {
    pub fn build(hist: &[Sample]) -> Self {
        let mut map: std::collections::HashMap<String, SidTrace> = std::collections::HashMap::new();
        for s in hist {
            let Some(k) = session_key(s.sid.as_deref()) else {
                continue;
            };
            if is_sub_sample(s) {
                map.entry(k).or_default().windows.push(s.t);
            } else if let Some(u) = s.usd.filter(|u| u.is_finite()) {
                map.entry(k).or_default().costs.push((s.t, u));
            }
        }
        SubSessions(map)
    }

    /// Whether this session was on a subscription around `t`: it reported window data
    /// within ±[`SUB_HOLD_SECS`]. Symmetric because a resume transient logs its restored
    /// cost *before* the windows repopulate, so the corroborating sample can land on either
    /// side of the sample being judged. Used to keep such a stray sample's dollars out of
    /// the spend aggregate however late the truth arrives.
    pub fn active_at(&self, key: &str, t: i64) -> bool {
        self.0.get(key).is_some_and(|tr| {
            tr.windows
                .iter()
                .any(|w| w.saturating_sub(t).saturating_abs() <= SUB_HOLD_SECS)
        })
    }

    /// Whether a cost-carrying payload with no `rate_limits` must still be treated as a
    /// subscription session, and if so how long its windows have been missing (for the
    /// status line's message). `None` means the session may be treated as API-key billing:
    /// either it never reported windows at all, or the switch is now proven.
    ///
    /// Proof is the session's own cost counter moving while no windows were reported. That
    /// is what separates the two ways a payload arrives in this shape: a resumed
    /// subscription session awaiting its first response has a *frozen* counter (no
    /// response, no cost), however long it idles, while a session billing to an API key
    /// spends continuously and is receiving responses that carry no windows at all.
    ///
    /// That movement must also have been going on for [`SUB_HOLD_SECS`], timed from the
    /// first observation *above* the restored counter. Timing it from the first window-less
    /// observation instead would let an idle resume spend the whole wait down while its
    /// counter sat frozen — and the wait exists for exactly the moment that follows, since
    /// cost and `rate_limits` need not land in the same payload and a single render can
    /// show fresh cost with the windows still missing. Anchored at the movement, releasing
    /// the hold requires cost to keep moving for 45 minutes without the windows ever coming
    /// back, which no subscription session does.
    pub fn hold(&self, key: &str, usd: Option<f64>, now: i64) -> Option<i64> {
        let tr = self.0.get(key)?;
        let last_window = *tr.windows.iter().max()?;
        let quiet = now.saturating_sub(last_window);
        let since = || tr.costs.iter().filter(|(t, _)| *t > last_window);

        // The counter as restored — the first window-less observation, not the last
        // windowed sample's value. Under the global throttle a session usually goes on
        // spending after its last logged sample, so a resumed counter sits legitimately
        // above the last one recorded with windows; that difference is evidence of nothing.
        let Some(&(_, restored)) = since().min_by_key(|(t, _)| *t) else {
            return Some(quiet); // nothing observed since the windows stopped
        };
        let Some(&(moved_at, first_move)) = since()
            .filter(|(_, u)| *u > restored)
            .min_by_key(|(t, _)| *t)
        else {
            return Some(quiet); // frozen counter: the resume transient, however long it lasts
        };

        // Measured against the first movement, not against `restored`: the counter must
        // have gone on climbing past that first bump. A single bump followed by silence is
        // the ambiguous render this wait exists for — and since the counter never falls,
        // testing against `restored` would stay satisfied forever once it happened, making
        // the rule "45 minutes after one bump" rather than "45 minutes of spending".
        let proven = now.saturating_sub(moved_at) >= SUB_HOLD_SECS
            && usd.is_some_and(|u| u.is_finite() && u > first_move);
        (!proven).then_some(quiet)
    }

    /// The counter on this session's most recent window-less cost sample, if it has one.
    pub fn last_cost(&self, key: &str) -> Option<f64> {
        let tr = self.0.get(key)?;
        tr.costs.iter().max_by_key(|(t, _)| *t).map(|&(_, u)| u)
    }

    /// Whether this session reported window data at any moment strictly between `a` and
    /// `b` — i.e. whether its cumulative cost counter passed through a subscription phase
    /// between two observations. Spend cannot be attributed across such a gap: the counter
    /// climbs there on the shadow estimate a subscription reports, not on money.
    pub fn windowed_between(&self, key: &str, a: i64, b: i64) -> bool {
        self.0
            .get(key)
            .is_some_and(|tr| tr.windows.iter().any(|&w| w > a && w < b))
    }

    /// The most recent moment this session reported window data at or before `t`.
    pub fn last_window_at(&self, key: &str, t: i64) -> Option<i64> {
        let tr = self.0.get(key)?;
        tr.windows.iter().copied().filter(|&w| w <= t).max()
    }

    /// `(time, was_subscription)` for this session's most recent sample.
    pub fn last_sample(&self, key: &str) -> Option<(i64, bool)> {
        let tr = self.0.get(key)?;
        let w = tr.windows.iter().copied().max();
        let c = tr.costs.iter().map(|(t, _)| *t).max();
        match (w, c) {
            (Some(w), Some(c)) => Some(if w >= c { (w, true) } else { (c, false) }),
            (Some(w), None) => Some((w, true)),
            (None, Some(c)) => Some((c, false)),
            (None, None) => None,
        }
    }

    /// Samples that ordinary retention must not evict, as `(session, time)`: for every
    /// session that has gone quiet, its newest windowed sample — the anchor
    /// [`hold`](Self::hold) reasons from — *and* the newest window-less one after it.
    ///
    /// Both, because they only work as a pair. Protecting the anchor alone buys one prune
    /// cycle: the cost sample that makes the session count as quiet is itself ordinary, so
    /// it goes first, and on the next write the session no longer looks quiet and the anchor
    /// is evicted normally. Losing it isn't a lost data point — with no windowed sample the
    /// session stops being held at all, and a subscriber's shadow cost is rendered as money.
    ///
    /// Bounded to the [`MAX_ANCHOR_SESSIONS`] most recently active quiet sessions. Sessions
    /// end while quiet all the time, so protecting every one forever would let the reserved
    /// set grow past the retention cap itself and the file with it; a time limit instead
    /// would only postpone the eviction this exists to prevent, since a session can be
    /// resumed long after its last sample. Oldest-quiet-first is the right thing to give up.
    pub fn anchors(&self) -> std::collections::HashSet<(String, i64)> {
        let mut quiet: Vec<(i64, &String, i64)> = self
            .0
            .iter()
            .filter_map(|(k, tr)| {
                let last_window = tr.windows.iter().copied().max()?;
                let newest_cost = tr
                    .costs
                    .iter()
                    .map(|(t, _)| *t)
                    .filter(|t| *t > last_window)
                    .max()?;
                Some((newest_cost, k, last_window))
            })
            .collect();
        quiet.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        quiet
            .into_iter()
            .take(MAX_ANCHOR_SESSIONS)
            .flat_map(|(cost_t, k, last_window)| [(k.clone(), last_window), (k.clone(), cost_t)])
            .collect()
    }
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

/// A healthy history is ≲230 KB (`MAX_ENTRIES_API` × ~140 B); anything vastly larger is
/// not ours, and parsing it (twice, on the lenient path) would burn render-blocking time
/// and memory. The next un-throttled `log_sample` rewrites the file pruned, so skipping an
/// oversized file self-heals within a minute.
const READ_CAP_BYTES: u64 = 4 << 20;

/// Read the history, returning an empty vec on any error (matches the Python's tolerance).
/// A single malformed element drops only itself, not the ~day of samples around it: the
/// strict parse is the fast path, and on failure each element is retried individually.
pub fn read_history(dir: &Path) -> Vec<Sample> {
    let path = history_path(dir);
    if fs::metadata(&path)
        .map(|m| m.len() > READ_CAP_BYTES)
        .unwrap_or(true)
    {
        return Vec::new();
    }
    let s = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str::<Vec<Sample>>(&s).unwrap_or_else(|_| {
        serde_json::from_str::<Vec<serde_json::Value>>(&s)
            .map(|vals| {
                vals.into_iter()
                    .filter_map(|v| serde_json::from_value(v).ok())
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Append one account-usage sample. Throttled, pruned, atomic. Best-effort (errors ignored).
pub fn log_sample(dir: &Path, input: &Value, now: i64) {
    // Subscription samples log as before; API-key samples only once `payload_mode` has seen
    // real positive cost (it never classifies a payload carrying a non-null `rate_limits`
    // value as ApiKey, so a subscription session's shadow cost cannot be recorded as spend).
    let mode = payload_mode(input);
    let mut api = match mode {
        Mode::Subscription => false,
        Mode::ApiKey => true,
        Mode::Unknown => return,
    };
    let usd = f64_at(input, &["cost", "total_cost_usd"]).filter(|u| u.is_finite());
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut hist = read_history(dir);
    let sid = session_key(input.get("session_id").and_then(|x| x.as_str()));
    let subs = SubSessions::build(&hist);

    // A sample that records a session *changing* billing shape is exempt from the shared
    // throttle slot. The throttle is global, and with several sessions rendering on the same
    // timer one can hold the slot indefinitely — which would be a lost data point for any
    // other sample, but here loses the only evidence a phase happened at all. Both
    // directions matter: without the windowed sample a subscription phase is invisible and
    // its shadow climb gets differenced into the day total; without the first probe a real
    // switch is never provable and the session stays on the waiting line for good. Bounded
    // to one extra write per minute per session, so a payload flapping between the two
    // shapes cannot outrun the retention cap.
    let changes_shape =
        sid.as_ref()
            .and_then(|k| subs.last_sample(k))
            .is_some_and(|(t, was_sub)| {
                was_sub != (mode == Mode::Subscription) && t <= now.saturating_sub(SAMPLE_INTERVAL)
            });
    if !changes_shape {
        if let Some(last) = hist.last() {
            // Overflow-safe (stored t is untrusted): equivalent to `now - last.t < INTERVAL`.
            if last.t > now.saturating_sub(SAMPLE_INTERVAL) {
                return; // too soon since the last sample
            }
        }
    }

    // A session that recently reported window data stays a subscription session until its
    // own cost counter proves the billing changed for good (see [`SubSessions::hold`]).
    // Until then the sample is still written, just demoted to a window-less *probe*: it can
    // never count as spend, and it is the observation that proof is later built from.
    // Dropping the sample instead — as this once did — left a switched session invisible in
    // the history and the question permanently unanswerable.
    if api {
        if let Some(k) = &sid {
            if subs.hold(k, usd, now).is_some() {
                api = false;
                // Only a counter that has *moved* is worth a slot. The first probe anchors
                // the comparison and every later movement extends it, but a resumed session
                // can sit frozen for hours, and repeating that value would churn ~1/min
                // through the retention cap — evicting the very window samples the hold
                // rests on, after which nothing holds the session at all.
                if usd.is_some() && subs.last_cost(k) == usd {
                    return;
                }
            }
        }
    }

    let five = nested(input, &["rate_limits", "five_hour"]);
    let week = nested(input, &["rate_limits", "seven_day"]);

    let sample = Sample {
        t: now,
        h5: five.and_then(get_pct),
        d7: week.and_then(get_pct),
        h5r: five.and_then(get_reset),
        d7r: week.and_then(get_reset),
        sid,
        usd,
        tin: nested(input, &["context_window", "total_input_tokens"]).and_then(|x| x.as_i64()),
        tout: nested(input, &["context_window", "total_output_tokens"]).and_then(|x| x.as_i64()),
        api,
        sub: false,
    };
    // Keep `payload_mode`'s verdict when the window values didn't survive parsing, so a
    // subscription sample is never stored in the shape that means the opposite.
    let sub = mode == Mode::Subscription && !has_window_data(&sample);
    hist.push(Sample { sub, ..sample });

    // Retention, mode-aware in both dimensions so subscription-only users keep exactly the
    // original behaviour: their 250 samples may legitimately span days (an every-other-day
    // user's weekly-window history anchors the report's $/1% estimate), so no age prune
    // applies. Once genuinely-API flagged samples exist, the age prune bounds the larger
    // file to the day window's actual lookback. A flagged sample whose sid later proved to
    // be a subscription session (resume transient) must not flip a Pro/Max machine's
    // retention — recomputed after the push so the just-logged sample counts as evidence.
    // The cutoff is hoisted so an absurd stored `t` can't overflow.
    let subs = SubSessions::build(&hist);
    let has_api = hist.iter().any(|s| {
        // Normalised, not the raw stored id: `SubSessions` is keyed on the 12-char
        // truncation, so an untruncated sid (a hand-edited or foreign file) would miss.
        s.api && !session_key(s.sid.as_deref()).is_some_and(|k| subs.active_at(&k, s.t))
    });
    // Both prunes spare the anchors: they are the evidence that a quiet session was ever on
    // a subscription, and dropping one hands that session to the API-key path outright.
    let anchors = subs.anchors();
    let is_anchor =
        |s: &Sample| session_key(s.sid.as_deref()).is_some_and(|k| anchors.contains(&(k, s.t)));
    if has_api {
        let cutoff = now.saturating_sub(MAX_AGE_SECS);
        hist.retain(|s| s.t > cutoff || is_anchor(s));
    }
    let cap = if has_api {
        MAX_ENTRIES_API
    } else {
        MAX_ENTRIES
    };
    if hist.len() > cap {
        let mut drop = hist.len() - cap;
        hist.retain(|s| {
            if drop == 0 || is_anchor(s) {
                return true;
            }
            drop -= 1;
            false
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = env::temp_dir().join(format!("quotaline-test-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sub_payload(usd: f64) -> Value {
        serde_json::json!({
            "session_id": "subsession-aaaa-bbbb",
            "cost": {"total_cost_usd": usd},
            "rate_limits": {"five_hour": {"used_percentage": 11, "resets_at": 9000}}
        })
    }

    fn api_payload(usd: f64) -> Value {
        serde_json::json!({
            "session_id": "apisession-cccc-dddd",
            "cost": {"total_cost_usd": usd, "total_duration_ms": 90_000}
        })
    }

    /// One session id under either auth mode — `--resume` reuses the id, so the same
    /// session can report windows for hours and then bill to an API key for the rest of
    /// its life.
    fn payload(sid: &str, usd: f64, windows: bool) -> Value {
        let mut v = serde_json::json!({
            "session_id": sid,
            "cost": {"total_cost_usd": usd}
        });
        if windows {
            v["rate_limits"] =
                serde_json::json!({"five_hour": {"used_percentage": 7, "resets_at": 9000}});
        }
        v
    }

    #[test]
    fn gating_matrix() {
        let d = TempDir::new("gate");
        // Subscription payload → logged, api=false, windows recorded.
        log_sample(&d.0, &sub_payload(3.5), 1000);
        // API payload with real cost → logged, api=true, dur captured, sid truncated.
        log_sample(&d.0, &api_payload(1.25), 1100);
        // Empty rate_limits + cost (subscription transient) → never logged as api.
        log_sample(
            &d.0,
            &serde_json::json!({"rate_limits": {}, "cost": {"total_cost_usd": 9.0}}),
            1200,
        );
        // API shape but zero cost → not logged.
        log_sample(
            &d.0,
            &serde_json::json!({"cost": {"total_cost_usd": 0, "total_duration_ms": 5}}),
            1300,
        );
        // Nothing usable → not logged.
        log_sample(&d.0, &Value::Null, 1400);

        let h = read_history(&d.0);
        assert_eq!(h.len(), 2, "only the sub and real-cost api payloads log");
        assert!(!h[0].api && h[0].h5 == Some(11.0) && h[0].usd == Some(3.5));
        assert!(h[1].api && h[1].usd == Some(1.25));
        assert_eq!(h[1].sid.as_deref(), Some("apisession-c")); // 12-char truncation
        assert!(is_api_sample(&h[1]) && !is_api_sample(&h[0]));
    }

    #[test]
    fn throttle_and_prune() {
        let d = TempDir::new("throttle");
        log_sample(&d.0, &api_payload(1.0), 1000);
        log_sample(&d.0, &api_payload(2.0), 1030); // 30s later: throttled away
        assert_eq!(read_history(&d.0).len(), 1);
        log_sample(&d.0, &api_payload(2.0), 1090); // 90s later: logged
        assert_eq!(read_history(&d.0).len(), 2);

        // Age prune: both samples are far older than MAX_AGE_SECS from this "now".
        log_sample(&d.0, &api_payload(3.0), 1000 + MAX_AGE_SECS + 10_000);
        let h = read_history(&d.0);
        assert_eq!(h.len(), 1, "stale samples age out");
        assert_eq!(h[0].usd, Some(3.0));
    }

    #[test]
    fn count_cap_is_mode_aware() {
        // Subscription-only histories keep the original tight cap.
        let d = TempDir::new("cap");
        let mut hist: Vec<Sample> = (0..MAX_ENTRIES_API)
            .map(|i| Sample {
                t: 10_000 + i as i64,
                h5: Some(1.0),
                ..Default::default()
            })
            .collect();
        let json = serde_json::to_string(&hist).unwrap();
        fs::write(d.0.join("usage-history.json"), &json).unwrap();
        log_sample(
            &d.0,
            &sub_payload(1.0),
            10_000 + MAX_ENTRIES_API as i64 + 100,
        );
        assert_eq!(read_history(&d.0).len(), MAX_ENTRIES);

        // One api sample in the mix → the larger cap applies.
        hist[0].api = true;
        hist[0].usd = Some(1.0);
        let json = serde_json::to_string(&hist).unwrap();
        fs::write(d.0.join("usage-history.json"), &json).unwrap();
        log_sample(
            &d.0,
            &sub_payload(1.0),
            10_000 + MAX_ENTRIES_API as i64 + 100,
        );
        assert_eq!(read_history(&d.0).len(), MAX_ENTRIES_API);
    }

    #[test]
    fn round_trip_and_leniency() {
        // Subscription samples serialize without the new keys (legacy shape preserved).
        let sub = vec![Sample {
            t: 5,
            h5: Some(1.0),
            usd: Some(2.0),
            ..Default::default()
        }];
        let json = serde_json::to_string(&sub).unwrap();
        assert!(
            !json.contains("api") && !json.contains("dur") && !json.contains("sub"),
            "{json}"
        );

        // Legacy JSON (no api) parses with defaults; unknown keys — including the retired
        // `dur` field this branch briefly wrote — are ignored.
        let legacy = r#"[{"t":1,"h5":2.0,"usd":3.0,"dur":90,"future_field":true}]"#;
        let h: Vec<Sample> = serde_json::from_str(legacy).unwrap();
        assert!(!h[0].api);

        // One malformed element (or an explicit "api":null) drops only itself.
        let d = TempDir::new("lenient");
        let mixed = r#"[{"t":1,"usd":1.0,"api":true},{"t":"bogus"},{"t":3,"usd":2.0,"api":null}]"#;
        fs::write(d.0.join("usage-history.json"), mixed).unwrap();
        let h = read_history(&d.0);
        assert_eq!(h.len(), 1, "good sample survives its bad neighbours");
        assert_eq!(h[0].t, 1);
    }

    #[test]
    fn stripped_samples_never_count_but_never_feed_subscription_maths() {
        // An old binary rewriting the file drops api/dur. The stripped sample must not
        // count as spend (that shape is identical to a degenerate subscription sample whose
        // windows didn't parse — counting it would fabricate dollars)…
        let stripped: Sample = serde_json::from_str(
            r#"{"t":1,"h5":null,"d7":null,"h5r":null,"d7r":null,"sid":"apisess","usd":12.4}"#,
        )
        .unwrap();
        assert!(!is_api_sample(&stripped));
        // …but it must also stay out of subscription-percentage maths, where excluding a
        // maybe-API sample is the safe direction.
        assert!(lacks_window_data(&stripped));

        // A subscription sample with any window trace is unambiguous on both counts.
        let sub: Sample =
            serde_json::from_str(r#"{"t":1,"h5":3.0,"sid":"subsess","usd":9.9}"#).unwrap();
        assert!(!is_api_sample(&sub));
        assert!(!lacks_window_data(&sub));

        // The explicit flag is what counts.
        let flagged = Sample {
            api: true,
            ..stripped.clone()
        };
        assert!(is_api_sample(&flagged));
    }

    #[test]
    fn durable_switch_logs_probes_first_then_spend() {
        let d = TempDir::new("switch");
        let sid = "switcher-1111-2222";
        // Hours on a subscription.
        log_sample(&d.0, &payload(sid, 20.0, true), 0);
        log_sample(&d.0, &payload(sid, 28.0, true), 10_000);
        // Resumed under an API key: same id, no windows, cost climbing. The samples still
        // land — demoted to window-less probes, which is what the proof is built from.
        log_sample(&d.0, &payload(sid, 28.0, false), 10_100);
        log_sample(&d.0, &payload(sid, 29.0, false), 11_000);
        let h = read_history(&d.0);
        assert_eq!(
            h.len(),
            4,
            "the switched session stays visible in the history"
        );
        assert!(
            h[2..].iter().all(|s| !s.api && lacks_window_data(s)),
            "cost that moved only moments ago proves nothing yet"
        );

        // Past the hold as timed from the first probe — but the wait runs from the first
        // *movement*, so it has not elapsed yet.
        log_sample(&d.0, &payload(sid, 32.0, false), 10_100 + SUB_HOLD_SECS);
        let h = read_history(&d.0);
        assert_eq!(h.len(), 5, "the sample was written, not thrown away");
        assert!(
            !h.last().unwrap().api,
            "the wait is timed from the movement, not the first sight of the session"
        );

        // Past it from the movement, with the counter proving money kept moving while no
        // windows were reported: the session is billing to an API key and its spend counts.
        log_sample(&d.0, &payload(sid, 33.0, false), 11_000 + SUB_HOLD_SECS);
        let h = read_history(&d.0);
        assert!(h.last().unwrap().api, "a proven switch logs as spend");
    }

    #[test]
    fn an_idle_resume_cannot_spend_the_wait_down_before_its_counter_moves() {
        let d = TempDir::new("idle-then-move");
        let sid = "idled-5555-6666";
        log_sample(&d.0, &payload(sid, 12.0, true), 0);
        // Resumed and left alone for hours: probes accumulate, counter frozen.
        for t in [100, 5_000, 20_000] {
            log_sample(&d.0, &payload(sid, 12.0, false), t);
        }
        // The first response finally arrives, and its cost can reach the payload a render
        // before `rate_limits` do. Timed from the first probe the wait would be long spent
        // and this single moment of movement would reclassify a Pro/Max session.
        log_sample(&d.0, &payload(sid, 12.4, false), 20_100);
        let h = read_history(&d.0);
        // Assert the write happened before reading anything into its flag: a sample the
        // throttle dropped would satisfy the `all(!api)` check for the wrong reason.
        assert_eq!(h.last().map(|s| s.usd), Some(Some(12.4)), "movement logged");
        assert!(
            h.iter().all(|s| !s.api),
            "a moment of movement after a frozen idle is not a switch"
        );
    }

    #[test]
    fn a_frozen_counter_never_proves_a_switch() {
        let d = TempDir::new("frozen");
        let sid = "resumed-3333-4444";
        log_sample(&d.0, &payload(sid, 12.0, true), 0);
        // Resumed and left alone: cost restored, `rate_limits` not yet, and the counter
        // unmoving because no response has arrived. However long that lasts, it is the
        // subscription transient rather than a change of billing.
        for t in [100, 5_000, 20_000, 100_000] {
            log_sample(&d.0, &payload(sid, 12.0, false), t);
        }
        let h = read_history(&d.0);
        assert!(h.iter().all(|s| !s.api), "no movement, no promotion — ever");
        // And the frozen value is recorded once, not once a minute: a session that idles
        // for hours must not churn its own window samples out of the retention cap, since
        // losing them is what would leave nothing holding it.
        assert_eq!(h.len(), 2, "one window sample, one probe");
    }

    #[test]
    fn a_subscription_payload_whose_windows_dont_parse_is_still_recorded_as_one() {
        // `rate_limits: {"five_hour": null}` is a shape this codebase already defends
        // against on the render side. Stored from the parsed values alone it is
        // indistinguishable from a hold probe — cost, no windows — and everything
        // downstream would read it as evidence of the opposite billing.
        let d = TempDir::new("nullwin");
        let sid = "nullwindows-1";
        let payload = serde_json::json!({
            "session_id": sid,
            "cost": {"total_cost_usd": 4.0},
            "rate_limits": {"five_hour": null, "seven_day": null}
        });
        assert_eq!(payload_mode(&payload), Mode::Subscription);
        log_sample(&d.0, &payload, 1000);
        let h = read_history(&d.0);
        assert!(h[0].sub && !h[0].api && !has_window_data(&h[0]));
        assert!(is_sub_sample(&h[0]), "the verdict survives the round trip");

        // So the session is still classified as a subscription: held, and its shadow cost
        // stays out of the day figure.
        let subs = SubSessions::build(&h);
        assert!(subs.active_at("nullwindows-", 1000));
        assert!(subs
            .hold("nullwindows-", Some(90.0), 1000 + 10 * SUB_HOLD_SECS)
            .is_some());
    }

    #[test]
    fn a_change_of_billing_shape_is_never_lost_to_the_shared_throttle() {
        // The throttle is global, so a busy peer session can hold the slot indefinitely.
        // Losing an ordinary sample to that is a lost data point; losing the one that marks
        // a phase boundary loses the only evidence the phase happened.
        let d = TempDir::new("shape");
        let sid = "shifter-1234";
        let peer = |t| log_sample(&d.0, &payload("peer-session", 1.0, true), t);
        log_sample(&d.0, &payload(sid, 5.0, true), 0);
        peer(1000); // peer takes the slot…
        log_sample(&d.0, &payload(sid, 5.0, false), 1030); // …but the switch still lands
        let h = read_history(&d.0);
        assert_eq!(h.len(), 3);
        assert!(
            lacks_window_data(h.last().unwrap()),
            "the probe was written"
        );

        // And back again: the windowed sample that ends the quiet period also lands.
        peer(1100);
        log_sample(&d.0, &payload(sid, 5.5, true), 1130);
        let h = read_history(&d.0);
        assert!(
            has_window_data(h.last().unwrap()),
            "the phase boundary was written"
        );

        // But an unchanged shape still waits its turn, so the exemption can't be a
        // back door around the throttle.
        peer(1200);
        log_sample(&d.0, &payload(sid, 6.0, true), 1230);
        assert_eq!(read_history(&d.0).len(), 6);
    }

    #[test]
    fn pruning_never_evicts_a_quiet_sessions_last_window_sample() {
        // Retention must not decide classification. Once that sample is gone there is
        // nothing left to hold the session, and a resumed Pro/Max session goes straight to
        // the API-key path with its shadow counter.
        let d = TempDir::new("anchor");
        let sid = "anchored-1234";
        log_sample(&d.0, &payload(sid, 5.0, true), 0); // the anchor
        log_sample(&d.0, &payload(sid, 5.0, false), 60); // and it goes quiet
                                                         // Another session then floods the file well past the cap.
        let flood: Vec<Sample> = (0..MAX_ENTRIES + 50)
            .map(|i| Sample {
                t: 1_000 + i as i64,
                h5: Some(3.0),
                sid: Some("noisy-session".into()),
                ..Default::default()
            })
            .collect();
        let mut hist = read_history(&d.0);
        hist.extend(flood);
        fs::write(
            d.0.join("usage-history.json"),
            serde_json::to_string(&hist).unwrap(),
        )
        .unwrap();
        // Several write cycles, not one. Protecting the window sample by itself survives the
        // first prune and then decays: the cost sample that marks the session quiet is
        // ordinary, so it goes first, and on the next pass the anchor no longer qualifies.
        for i in 0..4 {
            log_sample(
                &d.0,
                &payload("noisy-session", 1.0, true),
                100_000 + i * 100,
            );
            let after = read_history(&d.0);
            assert!(
                after.len() <= MAX_ENTRIES + 8,
                "cycle {i}: still pruned: {}",
                after.len()
            );
            assert!(
                after
                    .iter()
                    .any(|s| s.sid.as_deref() == Some("anchored-123") && has_window_data(s)),
                "cycle {i}: the quiet session's window sample survived the flood"
            );
            // And it still does its job: the session is held, not reclassified.
            assert!(
                SubSessions::build(&after)
                    .hold("anchored-123", Some(9.0), 100_000 + i * 100)
                    .is_some(),
                "cycle {i}: still held"
            );
        }
    }

    #[test]
    fn the_reserved_set_is_bounded_and_keeps_the_most_recent() {
        // Sessions end while quiet all the time. Reserving samples for every one of them
        // would grow past the retention cap itself and the file with it.
        let mut hist = Vec::new();
        for i in 0..(MAX_ANCHOR_SESSIONS as i64 * 3) {
            let sid = format!("session-{i:03}");
            hist.push(Sample {
                t: i * 100,
                h5: Some(5.0),
                sid: Some(sid.clone()),
                ..Default::default()
            });
            hist.push(Sample {
                t: i * 100 + 10,
                sid: Some(sid),
                usd: Some(1.0),
                ..Default::default()
            });
        }
        let anchors = SubSessions::build(&hist).anchors();
        assert_eq!(anchors.len(), MAX_ANCHOR_SESSIONS * 2, "two per session");
        // The newest quiet session is reserved; the oldest is given up first.
        let newest = format!("session-{:03}", MAX_ANCHOR_SESSIONS * 3 - 1);
        assert!(anchors.iter().any(|(k, _)| *k == newest));
        assert!(!anchors.iter().any(|(k, _)| k == "session-000"));
    }

    #[test]
    fn one_bump_then_silence_is_not_a_switch() {
        // The counter never falls, so a rule measured against the restored value would stay
        // satisfied forever after a single bump — 45 minutes later an idle Pro/Max session
        // would be reclassified and its shadow estimate presented as money.
        let hist = vec![
            Sample {
                t: 0,
                h5: Some(5.0),
                sid: Some("s".into()),
                usd: Some(10.0),
                ..Default::default()
            },
            Sample {
                t: 60,
                sid: Some("s".into()),
                usd: Some(10.0), // restored
                ..Default::default()
            },
            Sample {
                t: 120,
                sid: Some("s".into()),
                usd: Some(10.5), // one bump, then nothing ever again
                ..Default::default()
            },
        ];
        let subs = SubSessions::build(&hist);
        assert!(subs.hold("s", Some(10.5), 120 + SUB_HOLD_SECS).is_some());
        assert!(subs
            .hold("s", Some(10.5), 120 + 100 * SUB_HOLD_SECS)
            .is_some());
        // A forward clock step is not spending either.
        assert!(subs.hold("s", Some(10.5), i64::MAX).is_some());
        // Spending that actually continues does release it.
        assert_eq!(subs.hold("s", Some(11.0), 120 + SUB_HOLD_SECS), None);
    }

    #[test]
    fn a_repeated_probe_value_is_never_written_twice() {
        // The skip compares against the session's *newest* window-less counter. Against the
        // oldest, every repeat after a movement would be written — the ~1/min churn that
        // evicts the window samples the hold rests on.
        let d = TempDir::new("repeat");
        let sid = "repeating-9999";
        log_sample(&d.0, &payload(sid, 12.0, true), 0);
        log_sample(&d.0, &payload(sid, 12.0, false), 100); // anchor
        log_sample(&d.0, &payload(sid, 12.4, false), 200); // movement
        for t in [300, 400, 500, 600] {
            log_sample(&d.0, &payload(sid, 12.4, false), t); // and then quiet again
        }
        assert_eq!(read_history(&d.0).len(), 3);
    }

    #[test]
    fn a_counter_restored_above_the_last_windowed_one_is_not_movement() {
        // The global throttle means a session usually keeps spending after its last logged
        // sample, so a resumed counter sits legitimately above the last value recorded with
        // windows. Judging movement against *that* would make the very first probe look
        // like a switch, and 45 minutes later an idle Pro/Max session would be reclassified.
        let hist = vec![
            Sample {
                t: 0,
                h5: Some(5.0),
                sid: Some("s".into()),
                usd: Some(28.0), // last windowed counter
                ..Default::default()
            },
            Sample {
                t: 60,
                sid: Some("s".into()),
                usd: Some(30.0), // restored higher: $2 spent while unsampled
                ..Default::default()
            },
            Sample {
                t: 3_000,
                sid: Some("s".into()),
                usd: Some(30.0), // and frozen from there
                ..Default::default()
            },
        ];
        let subs = SubSessions::build(&hist);
        assert!(subs
            .hold("s", Some(30.0), 60 + 10 * SUB_HOLD_SECS)
            .is_some());
    }

    #[test]
    fn windows_returning_restart_the_proof_clock() {
        // Windows can lapse and come back mid-session. Evidence gathered while they were
        // missing says nothing once they return, or a session would be promoted moments
        // after proving itself a subscription.
        let win = |t| Sample {
            t,
            h5: Some(5.0),
            sid: Some("s".into()),
            usd: Some(10.0),
            ..Default::default()
        };
        let probe = |t, usd| Sample {
            t,
            sid: Some("s".into()),
            usd: Some(usd),
            ..Default::default()
        };
        let subs = SubSessions::build(&[win(0), probe(60, 10.0), probe(3_000, 11.0), win(3_100)]);
        assert_eq!(
            subs.hold("s", Some(12.0), 3_100 + SUB_HOLD_SECS),
            Some(SUB_HOLD_SECS),
            "the clock restarts from the windows coming back"
        );
    }

    #[test]
    fn any_window_field_marks_a_subscription_sample() {
        // A payload carrying only `seven_day` is still subscription-shaped; filed under
        // costs instead, it would become proof-of-switch material and leave the report's
        // $/1% anchor reading API-key dollars.
        for s in [
            Sample {
                d7: Some(1.0),
                ..Default::default()
            },
            Sample {
                h5r: Some(1.0),
                ..Default::default()
            },
            Sample {
                d7r: Some(1.0),
                ..Default::default()
            },
        ] {
            assert!(has_window_data(&s));
            assert!(!lacks_window_data(&Sample {
                usd: Some(1.0),
                ..s
            }));
        }
    }

    #[test]
    fn an_upgraded_history_keeps_its_pro_max_retention() {
        // Histories written by the shipped release contain exactly the stray
        // `api`-beside-windows samples the resume transient produced. One must not flip a
        // subscription-only machine onto the API retention regime, whose age prune would
        // discard the multi-day history the report's weekly $/1% anchor needs.
        let d = TempDir::new("upgrade");
        let seed = vec![
            Sample {
                t: 1000,
                sid: Some("subsession-a".into()),
                usd: Some(9.0),
                api: true, // the stray
                ..Default::default()
            },
            Sample {
                t: 1100,
                h5: Some(11.0),
                sid: Some("subsession-a".into()),
                usd: Some(9.0), // the truth, moments later
                ..Default::default()
            },
        ];
        fs::write(
            d.0.join("usage-history.json"),
            serde_json::to_string(&seed).unwrap(),
        )
        .unwrap();
        log_sample(&d.0, &sub_payload(10.0), 1100 + MAX_AGE_SECS + 1000);
        assert_eq!(
            read_history(&d.0).len(),
            3,
            "a day-old subscription history survives"
        );
    }

    #[test]
    fn subscription_classification_is_about_a_moment_not_a_session_id() {
        let win = |t| Sample {
            t,
            h5: Some(5.0),
            sid: Some("s".into()),
            usd: Some(1.0),
            ..Default::default()
        };
        let subs = SubSessions::build(&[win(1_000)]);
        // A sample taken beside the windows is shadowed on either side — the resume
        // transient logs its restored cost *before* they repopulate.
        assert!(subs.active_at("s", 1_000 - SUB_HOLD_SECS));
        assert!(subs.active_at("s", 1_000 + SUB_HOLD_SECS));
        // One taken a day later is a different session's worth of evidence.
        assert!(!subs.active_at("s", 1_000 + SUB_HOLD_SECS + 1));
        assert!(!subs.active_at("s", 100_000));
        assert!(!subs.active_at("never-seen", 1_000));
    }

    #[test]
    fn the_hold_lifts_only_on_time_and_movement_together() {
        let windowed = Sample {
            t: 0,
            h5: Some(5.0),
            sid: Some("s".into()),
            usd: Some(10.0),
            ..Default::default()
        };
        let probe = |t, usd| Sample {
            t,
            sid: Some("s".into()),
            usd: Some(usd),
            ..Default::default()
        };
        // Restored at $10 (the value movement is judged against) and frozen for hours —
        // the resume transient.
        let frozen = SubSessions::build(&[windowed.clone(), probe(60, 10.0), probe(20_000, 10.0)]);
        // The same session, its counter finally moving at t=20_000.
        let moving = SubSessions::build(&[windowed, probe(60, 10.0), probe(20_000, 10.5)]);

        // A session the history has never seen with windows is not held at all.
        assert_eq!(frozen.hold("fresh-api-sid", Some(1.0), 0), None);
        // A frozen counter is held however long it idles: the wait never starts.
        assert!(frozen
            .hold("s", Some(10.0), 20_000 + 10 * SUB_HOLD_SECS)
            .is_some());
        // Movement buys no credit for the idling that preceded it — the wait runs from
        // where the counter was first seen above the value it was restored at.
        assert!(moving
            .hold("s", Some(12.0), 20_000 + SUB_HOLD_SECS - 1)
            .is_some());
        // Both together release it.
        assert_eq!(moving.hold("s", Some(12.0), 20_000 + SUB_HOLD_SECS), None);
        // A live counter back below the restored value is not movement, whatever the
        // history holds; nor is a payload carrying no cost at all.
        assert!(moving
            .hold("s", Some(9.0), 20_000 + SUB_HOLD_SECS)
            .is_some());
        assert!(moving.hold("s", None, 20_000 + SUB_HOLD_SECS).is_some());
        // While held, the quiet time is measured from the last window data — what the
        // status line reports, and the only honest thing it knows.
        assert_eq!(frozen.hold("s", Some(10.0), 5_000), Some(5_000));
    }

    #[test]
    fn session_keys() {
        assert_eq!(
            session_key(Some("abcdefghijklmnop")).as_deref(),
            Some("abcdefghijkl")
        );
        assert_eq!(session_key(Some("short")).as_deref(), Some("short"));
        assert_eq!(session_key(Some("")), None);
        assert_eq!(session_key(None), None);
    }
}

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

/// A sample with cost but no trace of either subscription window. Not proof of API-key
/// spend (see [`is_api_sample`]) — but exactly the right boundary where *exclusion* is the
/// safe direction: such a sample's dollars may be stripped API spend, so they must not
/// feed subscription-percentage maths (the report's $/1% anchor).
pub fn lacks_window_data(s: &Sample) -> bool {
    s.usd.is_some() && s.h5.is_none() && s.d7.is_none() && s.h5r.is_none() && s.d7r.is_none()
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

/// Session ids that have ever logged a sample carrying subscription window data. On current
/// Claude Code, `claude --resume` restores a session's *cost* immediately but repopulates
/// `rate_limits` only from the first fresh API response — so a resumed Pro/Max session
/// briefly looks API-key-shaped. A session that ever reported window data is a subscription
/// session: both classification (render/log) and aggregation consult this.
pub fn subscription_sids(hist: &[Sample]) -> std::collections::HashSet<String> {
    hist.iter()
        .filter(|s| s.h5.is_some() || s.d7.is_some() || s.h5r.is_some() || s.d7r.is_some())
        .filter_map(|s| session_key(s.sid.as_deref()))
        .collect()
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
    let api = match payload_mode(input) {
        Mode::Subscription => false,
        Mode::ApiKey => true,
        Mode::Unknown => return,
    };
    let usd = f64_at(input, &["cost", "total_cost_usd"]).filter(|u| u.is_finite());
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut hist = read_history(dir);
    if let Some(last) = hist.last() {
        // Overflow-safe (stored t is untrusted): equivalent to `now - last.t < INTERVAL`.
        if last.t > now.saturating_sub(SAMPLE_INTERVAL) {
            return; // too soon since the last sample
        }
    }

    let sid = session_key(input.get("session_id").and_then(|x| x.as_str()));
    // A resumed subscription session looks API-key-shaped until its first fresh response
    // (cost restored, rate_limits not yet); if this sid ever logged window data, it is a
    // subscription session and must not be recorded as spend.
    let sub_sids = subscription_sids(&hist);
    if api && sid.as_ref().is_some_and(|k| sub_sids.contains(k)) {
        return;
    }

    let five = nested(input, &["rate_limits", "five_hour"]);
    let week = nested(input, &["rate_limits", "seven_day"]);

    hist.push(Sample {
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
    });

    // Retention, mode-aware in both dimensions so subscription-only users keep exactly the
    // original behaviour: their 250 samples may legitimately span days (an every-other-day
    // user's weekly-window history anchors the report's $/1% estimate), so no age prune
    // applies. Once genuinely-API flagged samples exist, the age prune bounds the larger
    // file to the day window's actual lookback. A flagged sample whose sid later proved to
    // be a subscription session (resume transient) must not flip a Pro/Max machine's
    // retention — recomputed after the push so the just-logged sample counts as evidence.
    // The cutoff is hoisted so an absurd stored `t` can't overflow.
    let sub_sids = subscription_sids(&hist);
    let has_api = hist
        .iter()
        .any(|s| s.api && !s.sid.as_ref().is_some_and(|k| sub_sids.contains(k)));
    if has_api {
        let cutoff = now.saturating_sub(MAX_AGE_SECS);
        hist.retain(|s| s.t > cutoff);
    }
    let cap = if has_api {
        MAX_ENTRIES_API
    } else {
        MAX_ENTRIES
    };
    if hist.len() > cap {
        let drop = hist.len() - cap;
        hist.drain(0..drop);
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
        assert!(!json.contains("api") && !json.contains("dur"), "{json}");

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

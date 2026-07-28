//! Loose accessors over the stdin payload — they mirror the original Python's defensive
//! `.get()` style so that a missing key or an unexpected type degrades to `None` rather
//! than ever breaking a render.

use serde_json::Value;

/// Walk a key path, yielding the value only if every step exists and the leaf is non-null.
pub fn nested<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key)?;
    }
    if cur.is_null() {
        None
    } else {
        Some(cur)
    }
}

/// A JSON number, or a numeric/epoch string — `None` for anything else.
pub fn as_f64_loose(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse::<f64>().ok()
            }
        }
        _ => None,
    }
}

/// String at a key path.
pub fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    nested(v, path).and_then(|x| x.as_str())
}

/// Number at a key path (loose).
pub fn f64_at(v: &Value, path: &[&str]) -> Option<f64> {
    nested(v, path).and_then(as_f64_loose)
}

/// Which billing signal the stdin payload carries.
///
/// Claude Code sends `rate_limits` for Pro/Max subscription accounts and omits the key
/// entirely for API-key billing — where `cost.total_cost_usd` is instead the session's
/// actual pay-as-you-go cost. There is no explicit auth-type field, so classification
/// leans on presence, conservatively:
///
/// - a non-empty `rate_limits` object → `Subscription`;
/// - `rate_limits` present but empty (`{}`) or wrongly typed → `Unknown` — a
///   subscription-side transient, so a sub session's shadow cost can't be mistaken for
///   real spend. An explicit `rate_limits: null` reads as absent (a serialised missing
///   object), not as a subscription signal;
/// - key absent (or null) and real cost accrued (finite, > 0) → `ApiKey`. Requiring
///   positive cost keeps the pre-first-response window (both billing types) on the
///   `Unknown`/"limits n/a" path, mirroring the subscription behaviour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Subscription,
    ApiKey,
    Unknown,
}

pub fn payload_mode(input: &Value) -> Mode {
    match input.get("rate_limits") {
        Some(Value::Object(m)) if !m.is_empty() => return Mode::Subscription,
        Some(Value::Null) | None => {}
        Some(_) => return Mode::Unknown,
    }
    let real_cost = matches!(nested(input, &["cost"]), Some(Value::Object(_)))
        && f64_at(input, &["cost", "total_cost_usd"]).is_some_and(|u| u.is_finite() && u > 0.0);
    if real_cost {
        Mode::ApiKey
    } else {
        Mode::Unknown
    }
}

/// Usage percentage from a rate-limit (or context) window object.
pub fn get_pct(window: &Value) -> Option<f64> {
    for k in ["used_percentage", "utilization", "percent"] {
        if let Some(x) = window.get(k) {
            if let Some(f) = as_f64_loose(x) {
                return Some(f);
            }
        }
    }
    None
}

/// Reset epoch (Unix seconds) from a rate-limit window object.
pub fn get_reset(window: &Value) -> Option<f64> {
    for k in ["resets_at", "reset_at", "resets"] {
        if let Some(x) = window.get(k) {
            if let Some(f) = as_f64_loose(x) {
                return Some(f);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_detection() {
        let sub: Value = serde_json::json!({
            "cost": {"total_cost_usd": 1.5},
            "rate_limits": {"five_hour": {"used_percentage": 11, "resets_at": 1785207000}}
        });
        assert_eq!(payload_mode(&sub), Mode::Subscription);

        // API-key billing: no rate_limits at all, and real cost accrued.
        let api: Value = serde_json::json!({
            "cost": {"total_cost_usd": 1.8125, "total_duration_ms": 326017}
        });
        assert_eq!(payload_mode(&api), Mode::ApiKey);

        // Neither (or degenerate) → unknown, the old "limits n/a" path.
        assert_eq!(payload_mode(&Value::Null), Mode::Unknown);
        assert_eq!(payload_mode(&serde_json::json!({})), Mode::Unknown);
        assert_eq!(
            payload_mode(&serde_json::json!({"rate_limits": {}, "cost": null})),
            Mode::Unknown
        );
    }

    #[test]
    fn mode_never_mistakes_subscription_for_api_key() {
        // A present-but-empty rate_limits object is a subscription-side transient: it must
        // not classify as ApiKey even with real accumulated (shadow) cost.
        assert_eq!(
            payload_mode(&serde_json::json!({"rate_limits": {}, "cost": {"total_cost_usd": 3.5}})),
            Mode::Unknown
        );
        // Wrong-typed rate_limits: still never ApiKey.
        assert_eq!(
            payload_mode(&serde_json::json!({"rate_limits": "x", "cost": {"total_cost_usd": 3.5}})),
            Mode::Unknown
        );
        // No rate_limits but zero cost: pre-first-response window for either billing type.
        assert_eq!(
            payload_mode(&serde_json::json!({"cost": {"total_cost_usd": 0}})),
            Mode::Unknown
        );
        // Non-finite cost (numeric string) must not enter ApiKey mode.
        assert_eq!(
            payload_mode(&serde_json::json!({"cost": {"total_cost_usd": "inf"}})),
            Mode::Unknown
        );
        // Explicit null rate_limits reads as absent: real cost → ApiKey.
        assert_eq!(
            payload_mode(
                &serde_json::json!({"rate_limits": null, "cost": {"total_cost_usd": 2.0}})
            ),
            Mode::ApiKey
        );
    }
}

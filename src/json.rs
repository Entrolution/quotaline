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

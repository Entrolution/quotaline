//! Burn-rate maths over the sample history: a least-squares %/hour slope restricted to the
//! current reset segment (so a window reset never reads as negative burn), the inline
//! status-line readout with its conditional cap-ETA warning, and the richer analysis the
//! `report` command prints.

use std::collections::HashMap;

use crate::fmt::{fmt_dur, fmt_rate, DIM, RED, RESET};
use crate::history::{Sample, RATE_WINDOW_MIN};

/// Which window's fields to read off a sample.
#[derive(Clone, Copy)]
pub enum Win {
    FiveHour,
    SevenDay,
}

impl Win {
    pub fn pct(self, s: &Sample) -> Option<f64> {
        match self {
            Win::FiveHour => s.h5,
            Win::SevenDay => s.d7,
        }
    }
    pub fn reset(self, s: &Sample) -> Option<f64> {
        match self {
            Win::FiveHour => s.h5r,
            Win::SevenDay => s.d7r,
        }
    }
}

/// Least-squares slope of pct vs. time, returned in %/hour.
pub fn slope_per_hr(points: &[(f64, f64)]) -> Option<f64> {
    let n = points.len();
    if n < 2 {
        return None;
    }
    let nf = n as f64;
    let mx = points.iter().map(|p| p.0).sum::<f64>() / nf;
    let my = points.iter().map(|p| p.1).sum::<f64>() / nf;
    let den: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
    if den == 0.0 {
        return None;
    }
    let num: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    Some(num / den * 3600.0)
}

/// Samples in the current reset segment with a present pct, then the recent sub-window.
fn segment(hist: &[Sample], win: Win, cur_reset: Option<f64>) -> Vec<&Sample> {
    hist.iter()
        .filter(|e| win.reset(e) == cur_reset && win.pct(e).is_some())
        .collect()
}

fn slope_over(seg: &[&Sample], win: Win, window_sec: f64, now: f64) -> Option<f64> {
    let recent: Vec<&Sample> = seg
        .iter()
        .copied()
        .filter(|e| now - e.t as f64 <= window_sec)
        .collect();
    let chosen: &[&Sample] = if recent.len() >= 2 { &recent } else { seg };
    if chosen.len() < 2 {
        return None;
    }
    let points: Vec<(f64, f64)> = chosen
        .iter()
        .map(|e| (e.t as f64, win.pct(e).unwrap_or(0.0)))
        .collect();
    slope_per_hr(&points)
}

/// %/hr over the recent window (status-line readout).
pub fn history_rate(hist: &[Sample], win: Win, cur_reset: Option<f64>, now: f64) -> Option<f64> {
    let seg = segment(hist, win, cur_reset);
    slope_over(&seg, win, RATE_WINDOW_MIN * 60.0, now)
}

/// Inline burn readout. Returns `(plain, coloured)`; `("", "")` when there is no rate yet.
/// `plain` is used only for width measurement.
pub fn burn_suffix(
    pct: Option<f64>,
    reset_epoch: Option<f64>,
    rate: Option<f64>,
    now: f64,
) -> (String, String) {
    let rate = match rate {
        Some(r) => r,
        None => return (String::new(), String::new()),
    };
    let body = format!("↑{}%/h", fmt_rate(rate.max(0.0)));
    let mut plain = body.clone();
    let mut colored = format!("{DIM}{body}{RESET}");

    if let (Some(p), Some(reset)) = (pct, reset_epoch) {
        if p < 100.0 && rate > 0.5 {
            let eta = (100.0 - p) / rate * 3600.0;
            let ttr = reset - now;
            if eta > 0.0 && eta < ttr {
                // hits the cap before the window resets
                let warn = format!("⚠ cap {}", fmt_dur(eta as i64));
                plain.push_str("  ");
                plain.push_str(&warn);
                colored.push_str("  ");
                colored.push_str(&format!("{RED}{warn}{RESET}"));
            }
        }
    }
    (plain, colored)
}

/// `$`/`raw-token` per 1% conversions, anchored over the whole segment.
pub struct Conv {
    pub usd_per_pct: Option<f64>,
    pub tok_per_pct: Option<f64>,
}

pub struct Analysis {
    pub cur: f64,
    pub reset: Option<f64>,
    pub rate: Option<f64>,
    pub conv: Option<Conv>,
}

/// Sum, across sessions, each session's (max − min) of a cumulative counter over the
/// entries — i.e. total burn of that counter during the interval.
fn sum_session_delta<F: Fn(&Sample) -> Option<f64>>(
    entries: &[&Sample],
    extract: F,
) -> Option<f64> {
    let mut by_sid: HashMap<Option<String>, (f64, f64)> = HashMap::new();
    for e in entries {
        if let Some(v) = extract(e) {
            if !v.is_finite() {
                continue; // a NaN/inf would poison the running min/max
            }
            let slot = by_sid.entry(e.sid.clone()).or_insert((v, v));
            if v < slot.0 {
                slot.0 = v;
            }
            if v > slot.1 {
                slot.1 = v;
            }
        }
    }
    if by_sid.is_empty() {
        return None;
    }
    Some(by_sid.values().map(|(lo, hi)| hi - lo).sum())
}

/// Per-window analysis for the report: current %, recent rate, and the cost/token anchors.
pub fn analyze(hist: &[Sample], win: Win, window_sec: f64, now: f64) -> Option<Analysis> {
    let latest = hist.last()?;
    let cur = win.pct(latest)?;
    let cur_reset = win.reset(latest);

    let seg = segment(hist, win, cur_reset);
    let rate = slope_over(&seg, win, window_sec, now);

    let mut conv = None;
    if seg.len() >= 2 {
        let first = win.pct(seg[0]).unwrap_or(0.0);
        let last = win.pct(seg[seg.len() - 1]).unwrap_or(0.0);
        let d_pct = last - first;
        if d_pct > 0.0 {
            let t0 = seg[0].t as f64;
            let t1 = seg[seg.len() - 1].t as f64;
            let window_entries: Vec<&Sample> = hist
                .iter()
                .filter(|e| {
                    let t = e.t as f64;
                    t >= t0 && t <= t1
                })
                .collect();
            let usd = sum_session_delta(&window_entries, |s| s.usd);
            let tin =
                sum_session_delta(&window_entries, |s| s.tin.map(|v| v as f64)).unwrap_or(0.0);
            let tout =
                sum_session_delta(&window_entries, |s| s.tout.map(|v| v as f64)).unwrap_or(0.0);
            let tok = tin + tout;
            conv = Some(Conv {
                usd_per_pct: usd.filter(|u| u.is_finite() && *u > 0.0).map(|u| u / d_pct),
                tok_per_pct: if tok > 0.0 { Some(tok / d_pct) } else { None },
            });
        }
    }
    Some(Analysis {
        cur,
        reset: cur_reset,
        rate,
        conv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slope_known() {
        // 10% → 25% over 1800s = 15% per 0.5h = 30%/h.
        let pts = [(0.0, 10.0), (900.0, 17.5), (1800.0, 25.0)];
        let r = slope_per_hr(&pts).unwrap();
        assert!((r - 30.0).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn slope_needs_two_points() {
        assert!(slope_per_hr(&[(0.0, 1.0)]).is_none());
        assert!(slope_per_hr(&[]).is_none());
    }

    #[test]
    fn slope_flat_is_zero() {
        let pts = [(0.0, 5.0), (600.0, 5.0), (1200.0, 5.0)];
        assert_eq!(slope_per_hr(&pts), Some(0.0));
    }

    fn sample(t: i64, h5: f64, h5r: f64) -> Sample {
        Sample {
            t,
            h5: Some(h5),
            h5r: Some(h5r),
            ..Default::default()
        }
    }

    #[test]
    fn rate_restricted_to_current_segment() {
        // An older segment (different reset) must not contaminate the current rate.
        let hist = vec![
            sample(0, 90.0, 1000.0),   // previous reset segment
            sample(100, 95.0, 1000.0), // previous reset segment
            sample(1000, 0.0, 5000.0), // current segment starts after reset
            sample(1900, 30.0, 5000.0),
        ];
        let r = history_rate(&hist, Win::FiveHour, Some(5000.0), 2000.0).unwrap();
        // 0→30 over 900s = 120%/h; the 90→95 prior-segment points are excluded.
        assert!((r - 120.0).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn burn_suffix_warns_only_before_reset() {
        // rising fast, far from reset → cap warning present
        let (plain, _) = burn_suffix(Some(50.0), Some(10_000.0), Some(60.0), 0.0);
        assert!(plain.contains("cap"), "{plain}");
        // same rate but reset is imminent → no warning (resets first)
        let (plain2, _) = burn_suffix(Some(50.0), Some(60.0), Some(60.0), 0.0);
        assert!(!plain2.contains("cap"), "{plain2}");
        // no rate yet → empty
        assert_eq!(burn_suffix(Some(50.0), Some(10_000.0), None, 0.0).0, "");
    }
}

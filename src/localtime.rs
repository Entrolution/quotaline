//! Local wall-clock conversion for the reset readout.
//!
//! Rust's standard library has no timezone support — `SystemTime` only yields a UTC epoch, and
//! std cannot tell you the local offset, let alone apply DST. So we call the C library's
//! reentrant `localtime_r` (unix) / `_localtime64_s` (Windows UCRT) — always linked, no external
//! crate — which reads the OS timezone database and returns local hour/minute/weekday with full
//! DST/zone correctness. This is the same path Python's `datetime.fromtimestamp` takes under the
//! hood — a faithful restore of the original.
//!
//! All release targets are 64-bit, where `time_t` is 64-bit, so the epoch is passed as `i64`.

/// Local (hour 0–23, minute 0–59, second 0–60, weekday 0=Sunday) for a UTC `epoch`, or `None`
/// if the C library rejects the value (out of range) or the platform has no `localtime` we know
/// how to call.
#[cfg(unix)]
pub fn local_hms(epoch: i64) -> Option<(i32, i32, i32, i32)> {
    use std::ffi::{c_char, c_int, c_long};

    // POSIX `struct tm`, including the BSD/glibc `tm_gmtoff`/`tm_zone` tail present on both macOS
    // and Linux — declared so the struct size matches what `localtime_r` writes.
    #[repr(C)]
    struct Tm {
        tm_sec: c_int,
        tm_min: c_int,
        tm_hour: c_int,
        tm_mday: c_int,
        tm_mon: c_int,
        tm_year: c_int,
        tm_wday: c_int,
        tm_yday: c_int,
        tm_isdst: c_int,
        tm_gmtoff: c_long,
        tm_zone: *const c_char,
    }
    extern "C" {
        fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
    }

    let t = epoch;
    let mut tm = std::mem::MaybeUninit::<Tm>::zeroed();
    // SAFETY: `t` outlives the call; `localtime_r` fills the `struct tm` it is given and returns
    // that pointer (null only on error). We read only the integer fields afterwards.
    let ok = unsafe { !localtime_r(&t, tm.as_mut_ptr()).is_null() };
    if !ok {
        return None;
    }
    let tm = unsafe { tm.assume_init() };
    Some((tm.tm_hour, tm.tm_min, tm.tm_sec, tm.tm_wday))
}

/// Windows uses the UCRT's `_localtime64_s`; its `struct tm` has no `tm_gmtoff`/`tm_zone` tail.
#[cfg(windows)]
pub fn local_hms(epoch: i64) -> Option<(i32, i32, i32, i32)> {
    use std::ffi::c_int;

    #[repr(C)]
    struct Tm {
        tm_sec: c_int,
        tm_min: c_int,
        tm_hour: c_int,
        tm_mday: c_int,
        tm_mon: c_int,
        tm_year: c_int,
        tm_wday: c_int,
        tm_yday: c_int,
        tm_isdst: c_int,
    }
    extern "C" {
        // errno_t _localtime64_s(struct tm* _Tm, const __time64_t* _Time); 0 on success.
        fn _localtime64_s(result: *mut Tm, time: *const i64) -> c_int;
    }

    let t = epoch;
    let mut tm = std::mem::MaybeUninit::<Tm>::zeroed();
    // SAFETY: `t` outlives the call; `_localtime64_s` fills `tm` and returns 0 on success.
    let rc = unsafe { _localtime64_s(tm.as_mut_ptr(), &t) };
    if rc != 0 {
        return None;
    }
    let tm = unsafe { tm.assume_init() };
    Some((tm.tm_hour, tm.tm_min, tm.tm_sec, tm.tm_wday))
}

#[cfg(not(any(unix, windows)))]
pub fn local_hms(_epoch: i64) -> Option<(i32, i32, i32, i32)> {
    None
}

/// Snap a candidate epoch to the local midnight nearest to it. Plain second-of-day
/// arithmetic misses local midnight by the shift amount whenever a DST transition falls
/// between the two instants, so the candidate is re-inspected and the residual removed.
/// In zones whose spring-forward lands exactly on midnight (Havana, Santiago, Cairo …)
/// 00:00 doesn't exist that day — subtracting the residual would land an hour into the
/// previous day (and can put the rollover in the past), so the subtraction is verified and
/// the candidate kept when no true midnight exists: the candidate is already the first
/// instant of that local day.
fn snap_to_local_midnight(candidate: i64) -> Option<i64> {
    let (h, m, s, _) = local_hms(candidate)?;
    let off = (h as i64) * 3600 + (m as i64) * 60 + (s as i64);
    if off == 0 {
        return Some(candidate);
    }
    if off > 43_200 {
        // Landed late in the previous local day; the day's first instant lies ahead (and
        // equals it even when a midnight transition means that instant reads as 01:00).
        return Some(candidate + (86_400 - off));
    }
    let snapped = candidate - off;
    match local_hms(snapped) {
        Some((0, 0, 0, _)) => Some(snapped),
        _ => Some(candidate), // midnight doesn't exist (DST gap at 00:00)
    }
}

/// Epoch of today's local midnight — the start of the "spent today" window.
pub fn local_midnight(now: f64) -> Option<f64> {
    let (h, m, s, _) = local_hms(now as i64)?;
    let sec_of_day = (h as i64) * 3600 + (m as i64) * 60 + (s as i64);
    snap_to_local_midnight(now as i64 - sec_of_day).map(|t| t as f64)
}

/// Epoch of the next local midnight after `now` — the "spent today" rollover. Derived with
/// its own snap rather than `local_midnight + 86_400`, since a DST transition later today
/// shifts the rollover but not the start.
pub fn next_local_midnight(now: f64) -> Option<f64> {
    let (h, m, s, _) = local_hms(now as i64)?;
    let sec_of_day = (h as i64) * 3600 + (m as i64) * 60 + (s as i64);
    snap_to_local_midnight(now as i64 - sec_of_day + 86_400).map(|t| t as f64)
}

/// The day window bounds `(start, rollover)`, falling back to UTC midnights when local
/// time is unavailable — a stable rollover beats none. The fallback is wholesale so the
/// pair can never mix a local start with a UTC rollover.
pub fn day_bounds(now: f64) -> (f64, f64) {
    if let (Some(start), Some(roll)) = (local_midnight(now), next_local_midnight(now)) {
        return (start, roll);
    }
    let utc_start = (now / 86_400.0).floor() * 86_400.0;
    (utc_start, utc_start + 86_400.0)
}

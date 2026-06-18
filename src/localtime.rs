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

/// Local (hour 0–23, minute 0–59, weekday 0=Sunday) for a UTC `epoch`, or `None` if the C library
/// rejects the value (out of range) or the platform has no `localtime` we know how to call.
#[cfg(unix)]
pub fn local_hms(epoch: i64) -> Option<(i32, i32, i32)> {
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
    Some((tm.tm_hour, tm.tm_min, tm.tm_wday))
}

/// Windows uses the UCRT's `_localtime64_s`; its `struct tm` has no `tm_gmtoff`/`tm_zone` tail.
#[cfg(windows)]
pub fn local_hms(epoch: i64) -> Option<(i32, i32, i32)> {
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
    Some((tm.tm_hour, tm.tm_min, tm.tm_wday))
}

#[cfg(not(any(unix, windows)))]
pub fn local_hms(_epoch: i64) -> Option<(i32, i32, i32)> {
    None
}

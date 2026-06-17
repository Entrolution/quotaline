//! ANSI colours and the small set of value formatters shared by the status line and the
//! report. Ports of the original Python helpers.

pub const RESET: &str = "\x1b[0m";
pub const DIM: &str = "\x1b[2m";
pub const BOLD: &str = "\x1b[1m";
pub const GRAY: &str = "\x1b[90m";
pub const GREEN: &str = "\x1b[32m";
pub const AMBER: &str = "\x1b[38;5;214m"; // 256-colour amber (warning band)
pub const RED: &str = "\x1b[31m";

// Colour bands (percent of allowance used).
pub const AMBER_AT: f64 = 80.0;
pub const RED_AT: f64 = 90.0;

pub fn color_for(pct: Option<f64>) -> &'static str {
    match pct {
        None => GRAY,
        Some(p) if p >= RED_AT => RED,
        Some(p) if p >= AMBER_AT => AMBER,
        Some(_) => GREEN,
    }
}

/// Duration in seconds → compact human string (e.g. `53m`, `6d3h`, `2h05m`).
pub fn fmt_dur(secs: i64) -> String {
    if secs <= 0 {
        return "now".to_string();
    }
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        if hours > 0 {
            format!("{days}d{hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if mins > 0 {
            format!("{hours}h{mins:02}m")
        } else {
            format!("{hours}h")
        }
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        "<1m".to_string()
    }
}

/// Token count → `457k` / `1.2M`.
pub fn fmt_tokens(n: f64) -> String {
    if n >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.0}k", n / 1e3)
    } else {
        format!("{}", n as i64)
    }
}

/// Burn rate → `30`, `4.3`, `2` (one decimal, trailing zeros trimmed).
pub fn fmt_rate(r: f64) -> String {
    let s = format!("{r:.1}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Integer with thousands separators (`264,000`). Rust's std has no grouped format.
pub fn group_thousands(n: f64) -> String {
    let neg = n < 0.0;
    let digits = format!("{:.0}", n.abs());
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if neg {
        out.push('-');
    }
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(fmt_dur(0), "now");
        assert_eq!(fmt_dur(-5), "now");
        assert_eq!(fmt_dur(59), "<1m");
        assert_eq!(fmt_dur(60), "1m");
        assert_eq!(fmt_dur(53 * 60), "53m");
        assert_eq!(fmt_dur(3600), "1h");
        assert_eq!(fmt_dur(2 * 3600 + 5 * 60), "2h05m");
        assert_eq!(fmt_dur(6 * 86_400 + 3 * 3600), "6d3h");
        assert_eq!(fmt_dur(2 * 86_400), "2d");
    }

    #[test]
    fn rates() {
        assert_eq!(fmt_rate(30.0), "30");
        assert_eq!(fmt_rate(4.3), "4.3");
        assert_eq!(fmt_rate(2.0), "2");
        assert_eq!(fmt_rate(0.0), "0");
    }

    #[test]
    fn tokens() {
        assert_eq!(fmt_tokens(457_000.0), "457k");
        assert_eq!(fmt_tokens(1_200_000.0), "1.2M");
        assert_eq!(fmt_tokens(512.0), "512");
    }

    #[test]
    fn thousands() {
        assert_eq!(group_thousands(264_000.0), "264,000");
        assert_eq!(group_thousands(17_600.0), "17,600");
        assert_eq!(group_thousands(999.0), "999");
        assert_eq!(group_thousands(0.0), "0");
    }

    #[test]
    fn color_bands() {
        assert_eq!(color_for(None), GRAY);
        assert_eq!(color_for(Some(10.0)), GREEN);
        assert_eq!(color_for(Some(85.0)), AMBER);
        assert_eq!(color_for(Some(95.0)), RED);
    }
}

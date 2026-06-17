//! The facelift: a smooth-fill progress bar — full blocks plus a fractional eighth-block
//! leading edge for sub-cell resolution — inside a slim dim frame `▕ … ▏`.

use crate::fmt::{color_for, DIM, GRAY, RESET};

const FULL: char = '█';
// index 1..=7 → ▏▎▍▌▋▊▉ (one-eighth … seven-eighths); index 0 is unused (no partial cell).
const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// The inner bar (no frame): `width` cells, coloured by `pct`. `None` → dim empty track.
fn inner(pct: Option<f64>, width: usize) -> String {
    let p = match pct {
        Some(p) => p.clamp(0.0, 100.0),
        None => return format!("{GRAY}{}{RESET}", " ".repeat(width)),
    };
    let total_eighths = ((p / 100.0) * width as f64 * 8.0).round() as i64;
    let total_eighths = total_eighths.clamp(0, width as i64 * 8);
    let full = (total_eighths / 8) as usize;
    let frac = (total_eighths % 8) as usize;

    let mut fill = String::new();
    for _ in 0..full {
        fill.push(FULL);
    }
    let mut used = full;
    if frac > 0 && used < width {
        fill.push_str(EIGHTHS[frac]);
        used += 1;
    }
    let empty = width.saturating_sub(used);
    format!("{}{fill}{RESET}{}", color_for(Some(p)), " ".repeat(empty))
}

/// Inner bar wrapped in the slim dim frame.
pub fn framed(pct: Option<f64>, width: usize) -> String {
    format!("{DIM}▕{RESET}{}{DIM}▏{RESET}", inner(pct, width))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip ANSI escape sequences so we can count visible cells.
    fn visible(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for n in chars.by_ref() {
                    if n == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn inner_cells(pct: Option<f64>, width: usize) -> String {
        visible(&framed(pct, width))
            .chars()
            .filter(|&c| c != '▕' && c != '▏')
            .collect()
    }

    #[test]
    fn full_is_all_blocks() {
        let cells = inner_cells(Some(100.0), 10);
        assert_eq!(cells.chars().count(), 10);
        assert_eq!(cells.chars().filter(|&c| c == '█').count(), 10);
    }

    #[test]
    fn empty_and_none_are_width() {
        assert_eq!(inner_cells(Some(0.0), 12).chars().count(), 12);
        assert_eq!(inner_cells(None, 12).chars().count(), 12);
        assert_eq!(
            inner_cells(Some(0.0), 12)
                .chars()
                .filter(|&c| c == '█')
                .count(),
            0
        );
    }

    #[test]
    fn fractional_keeps_width_and_partial() {
        // 31.25% of 8 cells = 20 eighths → 2 full blocks + a 4/8 (▌) partial; width preserved.
        let cells = inner_cells(Some(31.25), 8);
        assert_eq!(cells.chars().count(), 8);
        assert_eq!(cells.chars().filter(|&c| c == '█').count(), 2);
        assert!(
            cells.contains('▌'),
            "expected a 4/8 partial cell, got {cells:?}"
        );
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(
            inner_cells(Some(250.0), 6)
                .chars()
                .filter(|&c| c == '█')
                .count(),
            6
        );
        assert_eq!(
            inner_cells(Some(-5.0), 6)
                .chars()
                .filter(|&c| c == '█')
                .count(),
            0
        );
    }
}

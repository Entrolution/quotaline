//! Project-memory gauge.
//!
//! Claude Code auto-loads `MEMORY.md` every session but **head-truncates** it at 200 lines
//! or 25,000 UTF-16 code units (JS `.length`), silently dropping the tail past either cap.
//! This gauge shows how full the index is so you get a warning *before* memory stops
//! loading.
//!
//! The char margin tracks the harness's own threshold rather than a guess at a safe distance
//! from the cap. After every write to `MEMORY.md` Claude Code fires a `PostToolUse` hook
//! nagging that the index is "approaching the read limit", and that fires at 80% of the cap,
//! so `CHAR_BUDGET` is 80% and the gauge stops being green at the moment the harness starts
//! complaining. `LINE_BUDGET` keeps its old 190: no firing has ever cited the line dimension,
//! so there is nothing measured to move it to. The derivation is in the CHANGELOG entry that
//! set these, and `char_budget_tracks_the_measured_trigger` pins the ratio.

use std::path::{Path, PathBuf};

use crate::fmt::{AMBER, DIM, GREEN, RED, RESET};

pub const LINE_CAP: usize = 200;
pub const CHAR_CAP: usize = 25_000;
pub const LINE_BUDGET: usize = 190;
/// The harness's own "approaching the read limit" trigger, 80% of `CHAR_CAP`. Do not raise
/// this without new firing data: the gauge exists to warn before the harness does, and at
/// its previous 23,500 it did not.
pub const CHAR_BUDGET: usize = 20_000;

pub struct MemStat {
    pub lines: usize,
    /// UTF-16 code units, matching the JS `.length` that Claude Code truncates on.
    pub chars: usize,
}

/// `<…>/<project>/<session>.jsonl` → `<…>/<project>/memory/MEMORY.md`.
fn memory_path(transcript_path: &str) -> Option<PathBuf> {
    let parent = Path::new(transcript_path).parent()?;
    Some(parent.join("memory").join("MEMORY.md"))
}

/// Max bytes to read — far above any sane MEMORY.md. Bounds the live-render read so a
/// pathologically huge file can't OOM the status line; combined with the `is_file` check it
/// also avoids ever blocking on a pipe. The gauge only needs ~the first 200 lines / 25k units.
const READ_CAP: u64 = 1 << 20; // 1 MiB

/// Measure the current project's MEMORY.md, or `None` if there is no transcript path or file.
pub fn measure(transcript_path: Option<&str>) -> Option<MemStat> {
    use std::io::Read;
    let path = memory_path(transcript_path?)?;
    // Guard the live render path: only a regular file (metadata/stat never blocks), and a
    // bounded read — so a FIFO can't hang the line and a huge file can't OOM it.
    if !std::fs::metadata(&path).ok()?.is_file() {
        return None;
    }
    let mut content = String::new();
    std::fs::File::open(&path)
        .ok()?
        .take(READ_CAP)
        .read_to_string(&mut content)
        .ok()?;
    Some(MemStat {
        // split('\n') (not lines()) matches the JS line count Claude Code truncates on.
        lines: content.split('\n').count(),
        chars: content.encode_utf16().count(),
    })
}

/// 0 = within budget, 1 = approaching the cap (amber), 2 = at/over the cap (red, truncating).
pub fn level(stat: &MemStat) -> u8 {
    if stat.lines >= LINE_CAP || stat.chars >= CHAR_CAP {
        2
    } else if stat.lines >= LINE_BUDGET || stat.chars >= CHAR_BUDGET {
        1
    } else {
        0
    }
}

/// Header segment, e.g. `mem 71% (142ln)` — percentage is the binding dimension vs its cap.
pub fn header_segment(stat: &MemStat) -> String {
    let line_ratio = stat.lines as f64 / LINE_CAP as f64;
    let char_ratio = stat.chars as f64 / CHAR_CAP as f64;
    let pct = (line_ratio.max(char_ratio) * 100.0).round() as i64;
    let color = match level(stat) {
        2 => RED,
        1 => AMBER,
        _ => GREEN,
    };
    format!("{DIM}mem {RESET}{color}{pct}% ({}ln){RESET}", stat.lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(lines: usize, chars: usize) -> MemStat {
        MemStat { lines, chars }
    }

    #[test]
    fn levels() {
        assert_eq!(level(&stat(100, 12_000)), 0);
        assert_eq!(level(&stat(192, 12_000)), 1); // lines in the budget margin
        assert_eq!(level(&stat(100, 24_000)), 1); // chars in the budget margin
        assert_eq!(level(&stat(200, 12_000)), 2); // line cap → truncating
        assert_eq!(level(&stat(100, 25_500)), 2); // char cap → truncating
    }

    #[test]
    fn amber_starts_exactly_at_the_harness_trigger() {
        // The gauge must not be green while Claude Code is already asking for a compaction.
        assert_eq!(level(&stat(100, CHAR_BUDGET - 1)), 0);
        assert_eq!(level(&stat(100, CHAR_BUDGET)), 1);
        // The old 23,500 budget sat 3,500 units above the trigger: this is the regression.
        assert_eq!(level(&stat(100, 20_000)), 1);
        assert_eq!(level(&stat(100, 23_499)), 1);
    }

    #[test]
    fn char_budget_tracks_the_measured_trigger() {
        // 13 `PostToolUse` firings across three projects, 2026-07-04 to 2026-08-05. The
        // smallest reported "approaching" size was 19.5KB and 80% of the cap is 19.53KB, so
        // the harness warns at 20,000 units. Its stated compaction target, "under 17.1KB",
        // is 70% of the same cap, which corroborates the round-percentage reading.
        // Failing here means the budget moved without new firing data behind it.
        assert_eq!(CHAR_BUDGET, CHAR_CAP * 4 / 5);
    }

    #[test]
    fn header_uses_binding_dimension() {
        // chars dominate: 20000/25000 = 80% vs lines 100/200 = 50%. Note 20,000 is now also
        // the amber boundary, so this segment is amber; the assertions below are about the
        // percentage arithmetic, which is cap-denominated and independent of the colour.
        let seg = header_segment(&stat(100, 20_000));
        assert!(seg.contains("80%"), "{seg}");
        assert!(seg.contains("(100ln)"), "{seg}");
    }
}

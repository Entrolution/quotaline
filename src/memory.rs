//! Project-memory gauge.
//!
//! Claude Code auto-loads `MEMORY.md` every session but **head-truncates** it at 200 lines
//! or 25,000 UTF-16 code units (JS `.length`), silently dropping the tail past either cap.
//! This gauge shows how full the index is so you get a warning *before* memory stops
//! loading.
//!
//! The char margin tracks the harness's own threshold rather than a guess at a safe distance
//! from the cap. After every write to `MEMORY.md` Claude Code fires a `PostToolUse` hook
//! nagging that the index is "approaching the read limit". The smallest index ever observed
//! to trigger it sits just under 80% of the cap, close enough to read as a round-number
//! threshold, so `CHAR_BUDGET` is 80% and the gauge stops being green at about the moment the
//! harness starts complaining. That reading is an inference, not a number read out of the
//! harness: what is directly observed is an upper bound on the trigger, and the true value is
//! at or below where the budget now sits. `LINE_BUDGET` keeps its old 190: no firing has ever
//! cited the line dimension, so there is nothing measured to move it to. The derivation is in
//! the CHANGELOG entry that set these, and `char_budget_tracks_the_measured_trigger` pins the
//! ratio.

use std::path::{Path, PathBuf};

use crate::fmt::{AMBER, DIM, GREEN, RED, RESET};

pub const LINE_CAP: usize = 200;
pub const CHAR_CAP: usize = 25_000;
pub const LINE_BUDGET: usize = 190;
/// The harness's own "approaching the read limit" trigger, 80% of `CHAR_CAP`. Do not raise
/// this without new firing data: the gauge exists to warn before the harness does, and at
/// its previous 23,500 it did not.
pub const CHAR_BUDGET: usize = 20_000;

// `intuition.md`: the curated always-loaded index, mounted through `.claude/rules/`. These
// caps differ from the ones above in KIND, not just value. `MEMORY.md`'s are imposed by the
// harness and enforced by silent truncation; these are chosen by us, and the file is not
// size-capped at all (measured: a 34,649-char rules file loads in full). So over-cap here
// means "/dream will refuse to add and say so", which is a visible failure rather than a
// silently dropped tail.
pub const INT_CHAR_CAP: usize = 28_000;
/// The largest always-loaded index this store has actually run. A ceiling inside measured
/// territory rather than a round number, and a hard one: raising it needs evidence about
/// whether adherence degrades as the always-loaded set grows, which is unmeasured.
pub const INT_CHAR_BUDGET: usize = 25_000;
pub const INT_LINE_BUDGET: usize = 200;
/// Chars are the binding dimension here; the line rail is a sanity check. Set so the
/// budget-to-cap headroom matches the char dimension (200/224 == 25000/28000), which keeps
/// the two percentages comparable instead of having lines cry wolf first.
pub const INT_LINE_CAP: usize = 224;

/// One gauge's thresholds and label, so `MEMORY.md` and `intuition.md` share an
/// implementation rather than a copy of one.
struct Gauge {
    label: &'static str,
    line_cap: usize,
    char_cap: usize,
    line_budget: usize,
    char_budget: usize,
}

const MEM_GAUGE: Gauge = Gauge {
    label: "mem",
    line_cap: LINE_CAP,
    char_cap: CHAR_CAP,
    line_budget: LINE_BUDGET,
    char_budget: CHAR_BUDGET,
};

const INT_GAUGE: Gauge = Gauge {
    label: "int",
    line_cap: INT_LINE_CAP,
    char_cap: INT_CHAR_CAP,
    line_budget: INT_LINE_BUDGET,
    char_budget: INT_CHAR_BUDGET,
};

pub struct MemStat {
    pub lines: usize,
    /// UTF-16 code units, matching the JS `.length` that Claude Code truncates on.
    pub chars: usize,
}

/// `<…>/<project>/<session>.jsonl` → `<…>/<project>/memory/<file>`.
fn memory_path(transcript_path: &str, file: &str) -> Option<PathBuf> {
    let parent = Path::new(transcript_path).parent()?;
    Some(parent.join("memory").join(file))
}

/// Max bytes to read — far above any sane MEMORY.md. Bounds the live-render read so a
/// pathologically huge file can't OOM the status line; combined with the `is_file` check it
/// also avoids ever blocking on a pipe. The gauge only needs ~the first 200 lines / 25k units.
const READ_CAP: u64 = 1 << 20; // 1 MiB

fn measure_file(transcript_path: Option<&str>, file: &str) -> Option<MemStat> {
    use std::io::Read;
    let path = memory_path(transcript_path?, file)?;
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

/// Measure the current project's MEMORY.md, or `None` if there is no transcript path or file.
pub fn measure(transcript_path: Option<&str>) -> Option<MemStat> {
    measure_file(transcript_path, "MEMORY.md")
}

/// Measure the current project's `intuition.md`, or `None` when the project has no such file.
///
/// Returning `None` is the normal case: only a store that has been migrated to the curated
/// index has one, so every other project renders the `mem` gauge alone and is unaffected.
pub fn measure_intuition(transcript_path: Option<&str>) -> Option<MemStat> {
    measure_file(transcript_path, "intuition.md")
}

/// 0 = within budget, 1 = approaching the cap (amber), 2 = at/over the cap (red).
///
/// For `MEMORY.md` band 2 means the harness is truncating; for `intuition.md` it means
/// `/dream` will refuse to add. Different consequences, same three bands.
fn level_for(g: &Gauge, stat: &MemStat) -> u8 {
    if stat.lines >= g.line_cap || stat.chars >= g.char_cap {
        2
    } else if stat.lines >= g.line_budget || stat.chars >= g.char_budget {
        1
    } else {
        0
    }
}

fn segment_for(g: &Gauge, stat: &MemStat) -> String {
    let line_ratio = stat.lines as f64 / g.line_cap as f64;
    let char_ratio = stat.chars as f64 / g.char_cap as f64;
    let pct = (line_ratio.max(char_ratio) * 100.0).round() as i64;
    let color = match level_for(g, stat) {
        2 => RED,
        1 => AMBER,
        _ => GREEN,
    };
    let label = g.label;
    format!(
        "{DIM}{label} {RESET}{color}{pct}% ({}ln){RESET}",
        stat.lines
    )
}

/// Header segment, e.g. `mem 71% (142ln)` — percentage is the binding dimension vs its cap.
pub fn header_segment(stat: &MemStat) -> String {
    segment_for(&MEM_GAUGE, stat)
}

/// Header segment for `intuition.md`, e.g. `int 67% (128ln)`.
///
/// Deliberately identical in shape to `mem`: percentage against the cap, amber at the budget.
/// The two caps mean different things (one imposed, one chosen) but a reader should not have
/// to hold that distinction to compare two numbers printed side by side in the same format.
pub fn intuition_header_segment(stat: &MemStat) -> String {
    segment_for(&INT_GAUGE, stat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::{env, fs};

    fn stat(lines: usize, chars: usize) -> MemStat {
        MemStat { lines, chars }
    }

    /// Same throwaway-directory idiom as `history.rs`; the crate has no `tempfile` dev-dep.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = env::temp_dir().join(format!("quotaline-mem-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(p.join("proj/memory")).unwrap();
            TempDir(p)
        }
        /// The transcript path the status line is handed: `<…>/<project>/<session>.jsonl`.
        fn transcript(&self) -> String {
            self.0
                .join("proj/sess.jsonl")
                .to_string_lossy()
                .into_owned()
        }
        fn write(&self, name: &str, body: &str) {
            fs::write(self.0.join("proj/memory").join(name), body).unwrap();
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn levels() {
        assert_eq!(level_for(&MEM_GAUGE, &stat(100, 12_000)), 0);
        assert_eq!(level_for(&MEM_GAUGE, &stat(192, 12_000)), 1); // lines in the budget margin
        assert_eq!(level_for(&MEM_GAUGE, &stat(100, 24_000)), 1); // chars in the budget margin
        assert_eq!(level_for(&MEM_GAUGE, &stat(200, 12_000)), 2); // line cap → truncating
        assert_eq!(level_for(&MEM_GAUGE, &stat(100, 25_500)), 2); // char cap → truncating
    }

    #[test]
    fn amber_starts_exactly_at_the_harness_trigger() {
        // The gauge must not be green while Claude Code is already asking for a compaction.
        assert_eq!(level_for(&MEM_GAUGE, &stat(100, CHAR_BUDGET - 1)), 0);
        assert_eq!(level_for(&MEM_GAUGE, &stat(100, CHAR_BUDGET)), 1);
        // The old 23,500 budget sat 3,500 units above the trigger: this is the regression.
        assert_eq!(level_for(&MEM_GAUGE, &stat(100, 20_000)), 1);
        assert_eq!(level_for(&MEM_GAUGE, &stat(100, 23_499)), 1);
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
    fn intuition_gauge_has_its_own_thresholds() {
        // The point of a second gauge is that the same file size is judged differently. At
        // 22,000 chars the harness would already be nagging about a MEMORY.md, so mem is
        // amber; intuition.md is not subject to that cap and is still inside its own budget,
        // so int is green. Same number, two verdicts, which is exactly the distinction the
        // status line has to convey.
        assert_eq!(level_for(&MEM_GAUGE, &stat(128, 22_000)), 1);
        assert_eq!(level_for(&INT_GAUGE, &stat(128, 22_000)), 0);
        // And the real migrated index (18,828 / 128) is comfortably green on both.
        assert_eq!(level_for(&INT_GAUGE, &stat(128, 18_828)), 0);
        assert_eq!(level_for(&MEM_GAUGE, &stat(128, 18_828)), 0);

        assert_eq!(level_for(&INT_GAUGE, &stat(100, INT_CHAR_BUDGET - 1)), 0);
        assert_eq!(level_for(&INT_GAUGE, &stat(100, INT_CHAR_BUDGET)), 1);
        assert_eq!(level_for(&INT_GAUGE, &stat(100, INT_CHAR_CAP)), 2);
        assert_eq!(level_for(&INT_GAUGE, &stat(INT_LINE_BUDGET, 100)), 1);
        assert_eq!(level_for(&INT_GAUGE, &stat(INT_LINE_CAP, 100)), 2);
    }

    #[test]
    fn line_rail_does_not_cry_wolf_before_the_char_budget() {
        // Chars bind for intuition.md; the line rail is a sanity check. If the two dimensions
        // had different budget-to-cap headroom, a normal index would go amber on lines while
        // still well inside its char budget, and the percentage would stop meaning anything.
        let by_chars = INT_CHAR_BUDGET as f64 / INT_CHAR_CAP as f64;
        let by_lines = INT_LINE_BUDGET as f64 / INT_LINE_CAP as f64;
        assert!(
            (by_chars - by_lines).abs() < 0.01,
            "headroom mismatch: chars {by_chars:.3} vs lines {by_lines:.3}"
        );
    }

    #[test]
    fn intuition_segment_is_labelled_and_shaped_like_mem() {
        let seg = intuition_header_segment(&stat(128, 18_828));
        assert!(seg.contains("int "), "{seg}");
        assert!(seg.contains("(128ln)"), "{seg}");
        // 18828/28000 = 67%, and lines 128/224 = 57%, so chars bind.
        assert!(seg.contains("67%"), "{seg}");
        assert!(
            !seg.contains("mem"),
            "must not borrow the other gauge's label: {seg}"
        );
    }

    #[test]
    fn measure_reads_each_index_and_is_absent_when_the_file_is() {
        let t = TempDir::new("measure");

        // A store that has not been migrated: MEMORY.md only. This is every other project,
        // and it must render the mem gauge alone rather than an empty or bogus int one.
        t.write("MEMORY.md", "a\nb\nc\n");
        assert!(measure(Some(&t.transcript())).is_some());
        assert!(
            measure_intuition(Some(&t.transcript())).is_none(),
            "intuition gauge must be absent when the project has no intuition.md"
        );

        // A migrated store: both present, measured independently.
        t.write("intuition.md", "x\ny\n");
        let m = measure(Some(&t.transcript())).unwrap();
        let i = measure_intuition(Some(&t.transcript())).unwrap();
        assert_eq!((m.lines, m.chars), (4, 6));
        assert_eq!((i.lines, i.chars), (3, 4));

        assert!(measure(None).is_none());
        assert!(measure_intuition(None).is_none());
    }

    #[test]
    fn chars_are_utf16_units_not_bytes() {
        // The harness truncates on JS `.length`. A non-BMP char is 2 UTF-16 units and 4 UTF-8
        // bytes, so counting bytes would over-report and trip the gauge early.
        let t = TempDir::new("utf16");
        t.write("intuition.md", "𝄞"); // 1 scalar, 2 UTF-16 units, 4 bytes
        assert_eq!(measure_intuition(Some(&t.transcript())).unwrap().chars, 2);
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

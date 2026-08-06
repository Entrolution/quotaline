//! Project-memory gauge.
//!
//! Claude Code auto-loads `MEMORY.md` every session but **head-truncates** it once it exceeds
//! 200 lines or 25,000 UTF-16 code units (JS `.length`), silently dropping the tail past either
//! cap. This gauge shows how full the index is so you get a warning *before* memory stops
//! loading.
//!
//! Two details of that measurement the gauge has to match, or it reports on a file the harness
//! is not looking at. **Exceeds**: the predicate is `>`, so an index sitting exactly on a cap
//! loads whole and is not truncating. **Trimmed**: both dimensions are counted on
//! `content.trim()`, and the truncation window is applied to that same trimmed string, so
//! surrounding whitespace costs nothing and must cost nothing here either.
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

/// Trim the way JS `String.prototype.trim()` does, which is not quite what `str::trim` does.
///
/// Two characters differ, and both directions matter because the point of this module is to
/// report what the harness will do, not what Rust would:
/// - **U+FEFF** (BOM / ZWNBSP) — JS strips it, Rust does not. A BOM-prefixed index otherwise
///   blocks `trim_start` entirely, so every leading blank line survives and the count inflates.
/// - **U+0085** (NEL) — Rust strips it, JS does not. Plain `str::trim` would under-count here.
///
/// Neither appears in any index on this machine, and Claude Code's own writes produce neither,
/// so this buys fidelity rather than fixing an observed failure. It is three lines and removes
/// the need to reason about which of two trims is in play.
///
/// Note there is no correct answer for a fully empty index: the harness's caller guards with
/// `if (s.trim())` and substitutes a placeholder without measuring, so `SLt` never sees one.
/// Reporting 1 line / 0 chars is this crate's own convention, chosen because it falls out of
/// `split('\n')` and is harmless against every threshold.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c.is_whitespace() && c != '\u{0085}') || c == '\u{FEFF}')
}

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
    // Measure the TRIMMED content, because the harness does: its `SLt()` binds `t = e.trim()`
    // and counts on `t`, so surrounding whitespace is invisible to it. Counting it here made
    // every real index on this machine read as fuller than the harness considers it — checked
    // across 50 of them, all over-reported, most by one line, the worst by three.
    //
    // Trimming BOTH ends is deliberate, and the reason is not symmetric with the trailing case.
    // Trailing whitespace is trivially safe to drop: head-truncation would only have discarded
    // the empty tail anyway. Leading whitespace would NOT be safe if the harness truncated the
    // raw string, because head-truncation keeps the first N lines, so leading blanks would eat
    // the budget and drop real content — and this gauge would then under-report a warranted
    // red. It does not, and `Klr` is explicit about it: it destructures `trimmed: r` and slices
    // `r`, not the original — `let a = i ? r.split("\n").slice(0, rle).join("\n") : r`. The
    // window is applied to the trimmed string, so leading whitespace never costs a line.
    let trimmed = js_trim(&content);
    Some(MemStat {
        // split('\n') (not lines()) matches the JS `ju(t, "\n") + 1` the harness truncates on.
        // They agree on every input including CRLF, since both count only '\n' and both trims
        // strip the '\r'. On empty input both give 1 — see `js_trim` for why that number is
        // ours to choose rather than the harness's to dictate.
        lines: trimmed.split('\n').count(),
        chars: trimmed.encode_utf16().count(),
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
///
/// It shares `measure_file`, so it is trimmed on the same terms — but *not* for the same
/// reason. `intuition.md` is mounted through `.claude/rules/` and is not harness-size-capped
/// at all, so there is no truncation to predict here. Trimming it is a consistency choice: two
/// gauges side by side must count the same way, or the percentages are not comparable.
pub fn measure_intuition(transcript_path: Option<&str>) -> Option<MemStat> {
    measure_file(transcript_path, "intuition.md")
}

/// 0 = within budget, 1 = approaching the cap (amber), 2 = past the cap (red).
///
/// For `MEMORY.md` band 2 means the harness is truncating; for `intuition.md` it means
/// `/dream` will refuse to add. Different consequences, same three bands.
///
/// Band 2 is a strict `>`, matching the harness: `Klr` truncates on `n > rle` / `o > nPe`, so
/// a file of exactly 200 lines or exactly 25,000 units is loaded WHOLE. Using `>=` here showed
/// red — "the harness is truncating" — for a file it does not truncate. Band 1 keeps `>=`
/// because the budgets are our own thresholds and "approaching" is inclusive by construction.
fn level_for(g: &Gauge, stat: &MemStat) -> u8 {
    if stat.lines > g.line_cap || stat.chars > g.char_cap {
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
                                                                  // Exactly on the line cap is NOT truncating — the harness's predicate is `n > rle`.
                                                                  // This assertion previously expected 2 and was pinning the off-by-one it now guards.
        assert_eq!(level_for(&MEM_GAUGE, &stat(200, 12_000)), 1);
        assert_eq!(level_for(&MEM_GAUGE, &stat(201, 12_000)), 2); // past the line cap
        assert_eq!(level_for(&MEM_GAUGE, &stat(100, 25_500)), 2); // past the char cap
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
        // A migrated index of roughly the size this store runs is comfortably green on both.
        assert_eq!(level_for(&INT_GAUGE, &stat(128, 18_828)), 0);
        assert_eq!(level_for(&MEM_GAUGE, &stat(128, 18_828)), 0);

        assert_eq!(level_for(&INT_GAUGE, &stat(100, INT_CHAR_BUDGET - 1)), 0);
        assert_eq!(level_for(&INT_GAUGE, &stat(100, INT_CHAR_BUDGET)), 1);
        assert_eq!(level_for(&INT_GAUGE, &stat(INT_LINE_BUDGET, 100)), 1);

        // Band 2 is a strict `>`: exactly on a cap is still amber, one past it is red. For
        // MEM_GAUGE this is harness parity — `Klr` truncates on `n > rle` / `o > nPe`, so a
        // 200-line index loads whole and must not be reported as truncating. INT_GAUGE's caps
        // are self-imposed, but they share the predicate so they share the convention.
        assert_eq!(level_for(&MEM_GAUGE, &stat(LINE_CAP, 100)), 1);
        assert_eq!(level_for(&MEM_GAUGE, &stat(LINE_CAP + 1, 100)), 2);
        assert_eq!(level_for(&MEM_GAUGE, &stat(100, CHAR_CAP)), 1);
        assert_eq!(level_for(&MEM_GAUGE, &stat(100, CHAR_CAP + 1)), 2);
        assert_eq!(level_for(&INT_GAUGE, &stat(100, INT_CHAR_CAP)), 1);
        assert_eq!(level_for(&INT_GAUGE, &stat(100, INT_CHAR_CAP + 1)), 2);
        assert_eq!(level_for(&INT_GAUGE, &stat(INT_LINE_CAP, 100)), 1);
        assert_eq!(level_for(&INT_GAUGE, &stat(INT_LINE_CAP + 1, 100)), 2);
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
        // Trimmed: "a\nb\nc" is 3 lines / 5 units, "x\ny" is 2 / 3. The trailing newline is
        // not counted, because the harness does not count it either.
        assert_eq!((m.lines, m.chars), (3, 5));
        assert_eq!((i.lines, i.chars), (2, 3));

        assert!(measure(None).is_none());
        assert!(measure_intuition(None).is_none());
    }

    #[test]
    fn counts_ignore_surrounding_whitespace_because_the_harness_trims_first() {
        // The harness binds `t = e.trim()` before counting either dimension, so a file that
        // ends in blank lines is no fuller to it than the same file without them. Counting
        // them made this gauge disagree with the thing it exists to track — every real index
        // on this machine over-reported, most by one line.
        let t = TempDir::new("trim");

        t.write("MEMORY.md", "a\nb\nc");
        let tight = measure(Some(&t.transcript())).unwrap();

        // Same content, padded at both ends. Must measure identically.
        t.write("MEMORY.md", "\n\n  a\nb\nc\n\n\n");
        let padded = measure(Some(&t.transcript())).unwrap();
        assert_eq!(
            (tight.lines, tight.chars),
            (padded.lines, padded.chars),
            "padding changed the measurement; the harness would not have seen it"
        );
        assert_eq!((tight.lines, tight.chars), (3, 5));
    }

    #[test]
    fn trimming_follows_js_not_rust_on_the_two_chars_they_disagree_about() {
        // `js_trim` exists only for these two characters; without a test, reverting it to
        // `str::trim` would break nothing and the fidelity claim would be unfalsifiable.
        let t = TempDir::new("jstrim");

        // U+FEFF: JS strips it, Rust does not. Left unstripped it also blocks trim_start, so
        // the leading blank line survives and the count inflates as well as the char total.
        t.write("MEMORY.md", "\u{FEFF}\na\nb\n");
        let bom = measure(Some(&t.transcript())).unwrap();
        assert_eq!(
            (bom.lines, bom.chars),
            (2, 3),
            "a BOM must be stripped like JS does, not retained like str::trim does"
        );

        // U+0085 (NEL): Rust strips it, JS does not. It must survive, and it is one UTF-16
        // unit that the harness would count.
        t.write("MEMORY.md", "a\nb\u{0085}");
        let nel = measure(Some(&t.transcript())).unwrap();
        assert_eq!(
            (nel.lines, nel.chars),
            (2, 4),
            "NEL must be retained like JS does, not stripped like str::trim does"
        );
    }

    #[test]
    fn a_full_but_untruncated_index_reads_amber_not_red() {
        // The bug this module's trim fixes, stated as behaviour rather than as a count. Before
        // it, an index of LINE_CAP - 1 content lines with the ordinary trailing newline counted
        // LINE_CAP and went RED — "the harness is truncating" — for a file the harness reads in
        // full. Every other banding test builds `MemStat` by hand, so nothing else connects a
        // file on disk to a colour, and without this the fix could be silently undone.
        let t = TempDir::new("fullish");
        t.write("MEMORY.md", &"x\n".repeat(LINE_CAP - 1));

        let s = measure(Some(&t.transcript())).unwrap();
        assert_eq!(
            s.lines,
            LINE_CAP - 1,
            "the trailing newline must not add a line"
        );
        assert_eq!(
            level_for(&MEM_GAUGE, &s),
            1,
            "a file the harness loads whole must not be reported as truncating"
        );
    }

    #[test]
    fn an_empty_index_measures_as_one_line_like_the_harness() {
        // 1, not 0 — and by our own convention rather than harness parity. The harness never
        // measures an empty index at all: its caller guards with `if (s.trim())` and pushes a
        // "currently empty" placeholder instead, so there is no reference answer to match. 1 is
        // simply what `split('\n')` yields and is harmless against every threshold. Pinned so a
        // later switch to `lines()` (which yields 0) cannot change it unnoticed.
        let t = TempDir::new("empty");

        t.write("MEMORY.md", "");
        let empty = measure(Some(&t.transcript())).unwrap();
        assert_eq!((empty.lines, empty.chars), (1, 0));

        t.write("MEMORY.md", "\n\n  \n");
        let blank = measure(Some(&t.transcript())).unwrap();
        assert_eq!(
            (blank.lines, blank.chars),
            (1, 0),
            "a whitespace-only file trims to empty and measures the same"
        );
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

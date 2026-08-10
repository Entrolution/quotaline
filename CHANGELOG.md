# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **A session that switched billing mid-life could stay held forever, instead of for
  `SUB_HOLD_SECS`.** The hold that stops a resumed subscription session's shadow counter being
  presented as spend lifts only once that session's *own* cost is observed moving with no
  windows — which takes at least two observations, 45 minutes apart. Only the first of those
  was exempt from the shared throttle, via `changes_shape`; every later probe competed for a
  slot that is global across sessions.

  So the failure needed no unusual input, only a second busy session. A peer rendering on the
  same timer takes the slot every minute, the switched session is never observed moving, and
  the proof the hold waits for can never be assembled. The wait stops being a 45-minute one and
  becomes permanent: the status line shows `limits n/a` for the life of the session and real
  API-key spend never reaches the day total. Observed on a machine running four concurrent
  sessions, where the switched one logged its anchor and exactly one probe and then nothing.

  Later probes from a held session now take a per-session slot too, bounded exactly as
  `changes_shape` is — one extra write per minute per session — and gated on the counter having
  actually moved, so a frozen resume transient still cannot churn the retention cap.

## [1.3.1] - 2026-08-07

### Fixed

- **The memory gauges now measure a file the way Claude Code measures it.** Two disagreements,
  both making the gauge read fuller or redder than the harness ever would.

  *Whitespace.* The harness binds `t = content.trim()` and counts lines and UTF-16 units on
  `t`, and — this is the part that matters — applies the truncation window to that same trimmed
  string (`r.split("\n").slice(0, 200)`). This gauge counted the raw file. Since practically
  every index ends in a newline, practically every index over-reported: across the 50 on the
  development machine, all of them, most by one line and the worst by three. Trimming both ends
  is therefore safe, not just the trailing end: leading blank lines cannot consume the
  truncation budget because they are gone before the window is applied.

  *Boundary.* `level_for` used `>=` against the caps while the harness truncates on `>`. An
  index of exactly 200 lines or exactly 25,000 units loads **whole**, but the status line
  showed red — "the harness is truncating" — for it. Combined with the counting bug this was
  reachable by an ordinary file: 199 content lines plus the usual trailing newline counted 200
  and went red, with nothing actually being dropped. Band 1 keeps `>=`, because those
  thresholds are ours and "approaching" is inclusive by construction.

  Trimming now follows JS `String.prototype.trim()` rather than Rust's `str::trim`, which
  differ on U+FEFF (JS strips, Rust does not) and U+0085 (Rust strips, JS does not). Neither
  appears in any real index; this buys fidelity rather than fixing an observed failure.

  Displayed numbers move for most users — typically one line and one or two percentage points,
  always downward. No index on the development machine changes band as a result.

## [1.3.0] - 2026-08-06

### Added

- **`int N% (Xln)`, a second memory gauge**, shown next to `mem` when the project's memory
  directory contains an `intuition.md`. That file is a curated always-loaded index mounted
  through `.claude/rules/`, which the harness does not size-cap, so unlike `mem` its
  thresholds are chosen rather than imposed: amber at 25,000 characters or 200 lines, red at
  28,000. Red means the curation tooling will refuse to add, not that anything is being
  truncated. The line rail is set so its budget-to-cap headroom matches the char dimension
  (200/224 == 25000/28000), because chars are what bind and a mismatched line rail would go
  amber first and make the percentage meaningless.

  Both gauges now share one implementation behind a small `Gauge` descriptor instead of a
  copy, and the percentage stays cap-denominated in both so two numbers printed side by side
  in the same format mean the same thing. Projects without an `intuition.md` render exactly
  as before: `measure_intuition` returns `None` and the segment is omitted.

  Read the pair as a flow, inbox then curated store. Where the split is in use, a large `mem`
  means the inbox needs draining rather than the index being about to truncate.

### Fixed

- The memory gauge stayed **green while Claude Code was already asking for a compaction**.
  The amber threshold was 23,500 characters, chosen as a margin below the documented 25,000
  cap, but the harness starts nagging well before that. After each write to `MEMORY.md` it
  fires a hook saying the index is "approaching the 24.4KB read limit" and asking for it to be
  compacted "to under 17.1KB". Across 13 of those firings in three projects (2026-07-04 to
  2026-08-05) the smallest reported size was 19.5KB, against 19.53KB for 80% of the cap, and
  the stated 17.1KB target is 70.0% of the same cap. Two round percentages is what makes
  20,000 the reading, so treat it as an inference rather than a number read out of the
  harness. It is a safe one to act on: a nag only fires once the index is already past the
  line, so 19.5KB bounds the trigger from above and the true value is at or below the new
  budget. `CHAR_BUDGET` is
  now 20,000, so the gauge and the harness go amber together instead of the status line
  reading green through 3,500 units of nagging. `CHAR_CAP` stays at 25,000: that is still the
  documented truncation point and the right red line. The line margin is unchanged at 190,
  because no line-dimension trigger appears in any of the firings.

## [1.2.0] - 2026-08-01

### Fixed

- A session whose billing changed **mid-life** stayed on `limits n/a (awaiting first API
  response)` for the rest of its life. `claude --resume` reuses the session id, so a session
  started on a Pro/Max subscription and resumed from a shell exporting `ANTHROPIC_API_KEY`
  bills to that key from then on — and the guard keeping a *resumed* subscription session off
  the spend line had no expiry, so it mistook the permanent change for the transient it was
  written for. The session also stopped logging samples entirely, taking its real spend out
  of the day total with it.

  The hold now lifts once quotaline has watched that session's cost climb for 45 minutes with
  no `rate_limits` arriving. While held, samples still log, demoted to window-less probes that
  never count as spend and that the proof is built from.

- `quotaline report` blanked **both** the 5h and weekly sections whenever the newest sample in
  the history carried no window percentages, which a resumed session was enough to cause. It
  now selects the newest sample that actually reports a window, and the existing stale-reset
  check decides whether that window is still current.

- `limits n/a` no longer claims a session is "awaiting first API response" after hours of
  responses. Windows that stop arriving read as `limits n/a (none reported for 12m)`, and the
  header is kept, since that state can now last as long as the hold.

### Changed

- Whether a session counts as a subscription is decided **per moment** rather than per session
  id, which is what lets a session that switched auth contribute the dollars it spent
  afterwards.

- Because one cumulative cost counter spans a change of billing, each session's observations
  are differenced in contiguous **runs**, re-basing wherever it reported windows in between.
  Without this a session that ran on an API key, moved to a subscription and came back had the
  whole subscription phase's shadow estimate differenced into the day total as if it were
  money.

- A sample that records a change of billing shape is exempt from the shared sample throttle,
  bounded to one per minute per session. The throttle is global, so a busy peer session could
  otherwise hold the slot and drop the only evidence a phase happened.

- Pruning reserves the window sample a hold rests on, plus the cost sample marking the session
  quiet, for the most recently active such sessions. Retention must not decide classification.

- The day figure undercounts around a change of billing: spend during the hold is not
  attributable to either plan, so a session that flips auth once loses roughly 45 minutes of
  its burn from that day's total. This is the module's existing undercount-rather-than-guess
  bias, now documented with the right bound.

### Security

- No user-facing security fixes in this release.

- Hardening relevant to accounting integrity: a subscription payload whose `rate_limits`
  object carries no readable window values (a null or renamed inner field, or a shape this
  version doesn't know) is now recorded as the subscription sample it is. Stored from the
  parsed values alone it was byte-identical to a hold probe — cost with no windows — so a
  schema change on Anthropic's side could have caused a subscriber's estimated cost to be
  presented and aggregated as real spend.

### Compatibility

- An older quotaline binary reading a history written by this version leaves its `report`'s
  5h/weekly sections blank while a probe is the newest entry. No probe shape avoids this: the
  only samples an old binary's report skips are exactly the ones its day accounting counts as
  money. Status lines are unaffected, and upgrading resolves it.

- This version reads histories written by 1.0.0 and 1.1.0 unchanged, and an ordinary
  subscription sample still serialises to the same shape it always has.

## [1.1.0] - 2026-07-28

### Added

- **API-key (pay-as-you-go) mode**, for projects billed through `ANTHROPIC_API_KEY` or
  `apiKeyHelper`, where Claude Code sends no `rate_limits` and the status line previously
  showed `limits n/a` forever:
  - the session's cost joins the header (`… · ctx 7% (67k) · $1.81 · mem …`);
  - a `day` line shows this machine's spend today across all API-key sessions, a live
    `↑$X/h` burn rate, and the local-midnight rollover;
  - `QUOTALINE_DAILY_BUDGET` (a plain number, USD) turns the day line into a bar — % of
    budget, green/amber/red bands, `⚠ budget <eta>` when the current rate would hit the
    budget before midnight, and `⚠ over budget` once spent;
  - `quotaline report` gains a matching day section (spend, $/hr, budget ETA).

### Changed

- History retention is mode-aware: subscription-only machines keep the original small file.
- Day boundaries are DST-correct, including zones whose transition lands on midnight.
- Subscription (Pro/Max) rendering is untouched — output is byte-identical to 1.0.0.

### Fixed

- One corrupt history entry no longer discards the whole file.
- `report --window` validates its argument.

## [1.0.0] - 2026-06-17

### Added

- Initial release: a zero-token Claude Code status line showing the account's 5-hour and
  weekly usage windows as bars, with time-to-reset, a least-squares burn rate, and a cap-ETA
  warning when the current rate would exhaust a window before it resets.
- `quotaline report` — an on-demand burn-rate and headroom report.
- A project-memory gauge in the header, and self-install/uninstall that wires the status line
  into `~/.claude/settings.json`.
- Release pipeline and binary-downloader installers for macOS, Linux and Windows.

[Unreleased]: https://github.com/Entrolution/quotaline/compare/v1.3.1...HEAD
[1.3.1]: https://github.com/Entrolution/quotaline/compare/v1.3.0...v1.3.1
[1.3.0]: https://github.com/Entrolution/quotaline/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/Entrolution/quotaline/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/Entrolution/quotaline/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/Entrolution/quotaline/releases/tag/v1.0.0

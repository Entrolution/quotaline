# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/Entrolution/quotaline/compare/v1.2.0...HEAD
[1.2.0]: https://github.com/Entrolution/quotaline/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/Entrolution/quotaline/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/Entrolution/quotaline/releases/tag/v1.0.0

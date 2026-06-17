# quotaline

A **Claude Code status line** that shows your account-wide usage limits — the 5-hour
and weekly (7-day) windows — right in your prompt, with a live **burn rate** and a
warning when you're on track to hit a cap before it resets.

```
Opus 4.8 (1M context) · effort: max · ctx 46% (457k)
5h ████████░░░░░░░░░░░░░░░░░░░░░░  25%  resets in 53m @ 10:14pm (Wed)  +30%/h
wk ███░░░░░░░░░░░░░░░░░░░░░░░░░░░   9%  resets in 6d3h @ 1:20am (Wed)  +2%/h  cap 1d21h!
```

Plus an on-demand report (`burn.py`) with an approximate **$ headroom** estimate.

## Why this design — no token, no API calls, no ToS risk

This is the whole point, so it goes first:

- **It never touches your auth token and never calls `api.anthropic.com`.** It reads
  *only* the `rate_limits` object that Claude Code already pipes to status-line scripts
  on stdin. Zero network, zero credentials, nothing to leak.
- That matters because reusing a Pro/Max subscription OAuth token "in any other
  product, tool or service" is a **Consumer Terms violation** — the basis on which
  proxy-based and token-scraping usage trackers operate. This tool sidesteps that
  entirely: it's just reading data Claude Code hands it.
- **One line = your whole account.** The 5h and weekly limits are account-wide (shared
  across every session, every surface), so a single status line already reflects your
  total. There is nothing to aggregate across sessions.
- **No dependencies.** Pure Python 3 standard library. Two small files.

## What it shows

**Header** (omitted if your build doesn't provide these): the model, the current
reasoning `effort` level, and this session's context-window fill. `ctx` is colored by
absolute size — amber past 200k tokens, red past 500k — because that's what drives
per-turn cost, not the percentage.

**Two usage bars**, 5-hour and weekly, that grow to fill your terminal width:

- The bar and percentage are **green < 80%, amber 80–89%, red ≥ 90%**, tracking the
  live value — so when a window resets, it drops straight back to green.
- `resets in <duration> @ <local time> (<day>)` — when that window next resets.
- `+X%/h` — the **burn rate**, a least-squares fit over recent samples within the
  current reset segment (so a reset never reads as negative burn).
- `cap <eta>!` (red) — shown **only** when, at the current rate, you'd hit 100%
  *before* the window resets. No warning means you'll reset before you run out.

Before your plan/session produces usage data, the line shows:

```
limits n/a (awaiting first API response)
```

## Install

Requires a recent **Claude Code** (the status-line input must include `rate_limits`)
and a **Pro or Max** plan. macOS or Linux. Python 3 (standard library only).

```sh
git clone https://github.com/Entrolution/quotaline.git
cd quotaline
./install.sh
```

`install.sh` finds your `python3`, resolves the absolute path to `statusline.py`, backs
up `~/.claude/settings.json`, and **merges** a `statusLine` block into it without
touching your other settings. It's safe to re-run (idempotent), and it refuses to write
if your settings file isn't valid JSON. Start a new session (or wait ~10s) and the line
appears.

It writes a block like this:

```json
"statusLine": {
  "type": "command",
  "command": "/usr/local/bin/python3 /path/to/quotaline/statusline.py",
  "refreshInterval": 10
}
```

> Prefer to wire it by hand? Add the block above to `~/.claude/settings.json` yourself,
> using absolute paths to your `python3` and `statusline.py`.

To remove it (also backs up first):

```sh
./install.sh uninstall
```

## `burn.py` — on-demand burn-rate & headroom report

The status line appends a usage sample to `.state/usage-history.json` on each render
(throttled to ~1/min). `burn.py` reads that history and prints a fuller breakdown:

```sh
python3 burn.py                 # uses the last ~2h for the rate
python3 burn.py --window 60     # use the last 60 minutes instead
```

```
Claude usage — burn rate  (5 samples over 30m)

  5h  ████░░░░░░░░░░   25%  +30.0%/hr   ETA 2h30m   resets in 53m  → resets first
      headroom ~$11.00   ($0.147/1%, ≈17,600 raw-tok/1%)

  wk  █░░░░░░░░░░░░░    9%  +2.0%/hr   ETA 1d21h   resets in 6d3h  → hits cap first
      headroom ~$200.20   ($2.200/1%, ≈264,000 raw-tok/1%)

  % and ETA are exact (account-wide). $/token are estimates — they assume your
  usage is mostly these sessions; claude.ai etc. moves % but isn't logged.
  raw-tok counts include cache re-reads, so the $ figure is the steadier one.
```

The **% and ETA are exact**. The **`$` headroom is an estimate**: it anchors a "$ per
1%" rate on the cost burned by *these* Claude Code sessions, so usage elsewhere
(claude.ai, other tools) moves the percentage without showing up in the cost log. The
5h and weekly conversions are computed separately, since the same spend moves the two
windows by different amounts. Treat the dollar figure as a ballpark.

## Configuration

`refreshInterval` (seconds, in the `statusLine` block) is how often the line re-renders
even when idle, so the countdowns tick. Raise it to reduce churn; remove it for
event-driven-only updates.

A few knobs at the top of each file:

| Setting | File | Default | Meaning |
|---|---|---|---|
| `AMBER_AT` / `RED_AT` | `statusline.py` | `80` / `90` | usage-bar color thresholds (%) |
| `CTX_AMBER_TOK` / `CTX_RED_TOK` | `statusline.py` | `200k` / `500k` | context-size color thresholds (tokens) |
| `MAX_BAR` | `statusline.py` | `150` | max bar width on ultrawide terminals |
| `SAFE_MARGIN` | `statusline.py` | `8` | columns kept free at the right edge |
| `RATE_WINDOW_MIN` | `statusline.py` | `120` | trailing window for the inline burn rate |
| `SAMPLE_INTERVAL` | `statusline.py` | `60` | min seconds between logged samples |
| `MAX_ENTRIES` | `statusline.py` | `250` | history is pruned to the last N samples |
| `DEFAULT_WINDOW_MIN` | `burn.py` | `120` | default `--window` for the report |

Environment overrides:

- `CTT_STATE_DIR` — where the history/state lives (default: `.state/` next to the
  scripts). Handy for testing without touching your real data.
- `CLAUDE_SETTINGS`, `CTT_REFRESH_INTERVAL` — change which settings file `install.sh`
  writes to, and the `refreshInterval` it sets.

## How it works

Claude Code runs the `statusLine` command on each render and pipes a JSON object to it
on stdin. This tool reads the fields it needs (`rate_limits`, `context_window`, `model`,
`effort`, `cost`), renders the three lines, and — *after* flushing output, so it can
never delay your prompt — appends one usage sample to `.state/usage-history.json`. That
write is throttled, pruned, file-locked against concurrent sessions, and done via atomic
replace. The whole script is wrapped so a failure prints nothing rather than breaking
your status line.

## Platform support

- **macOS / Linux** — fully supported and tested.
- **Windows** — the status line runs (the POSIX-only `fcntl` lock is skipped, so history
  logging is best-effort), but it's not regularly tested, and `install.sh` is a bash
  script — you'd wire the `statusLine` block into `settings.json` manually.

## Notes & limits

- `rate_limits` is emitted only for **Pro/Max** accounts, and only **after the session's
  first API response**. Until then (or on a plan/version that doesn't send it) the line
  shows `limits n/a`. Each window can be independently absent.
- Anthropic doesn't publish the absolute token caps, so this shows **% of your allowance
  consumed**, not raw counts — which is the gauge you actually want.
- `.state/` is per-machine runtime data (including your usage history). It's
  `.gitignore`d; never commit it. Safe to delete — it just resets the history.

## License

[MIT](LICENSE)

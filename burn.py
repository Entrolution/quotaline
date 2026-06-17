#!/usr/bin/env python3
"""
burn.py — approximate Claude usage burn-rate + headroom report.

Reads the sample history written by statusline.py (.state/usage-history.json) and
prints, per window (5-hour and weekly):

  - current % used
  - burn rate in %/hour (least-squares over a recent window, restricted to the
    current reset segment so a window reset never reads as negative burn)
  - ETA to the 100% cap at that rate, vs. time until the window resets
  - approximate headroom in $ (cost-anchored), plus a rough raw-token figure

The % and ETA numbers are exact (the percentage is account-wide, covering every
session). The $/token figures are ESTIMATES: they anchor "$ per 1%" on the cost
burned by these Claude Code sessions, so anything you spend elsewhere (claude.ai,
other tools) moves the % but isn't in this log — treat them as ballpark. The 5h and
weekly conversions are computed separately (the same spend moves the two windows by
different amounts).

Usage:  python3 burn.py [--window MINUTES]     # recent window for the rate (default 120)
"""

import sys
import os
import json
import time

STATE_DIR = os.environ.get("CTT_STATE_DIR") or os.path.join(
    os.path.dirname(os.path.abspath(__file__)), ".state")
HISTORY_FILE = "usage-history.json"
DEFAULT_WINDOW_MIN = 120

RESET = "\033[0m"
DIM = "\033[2m"
BOLD = "\033[1m"
GRAY = "\033[90m"
GREEN = "\033[32m"
AMBER = "\033[38;5;214m"   # 256-colour amber (warning band)
RED = "\033[31m"


def color_for(pct):
    if pct is None:
        return GRAY
    if pct >= 90:
        return RED
    if pct >= 80:
        return AMBER
    return GREEN


def bar(pct, width=14):
    p = max(0.0, min(100.0, float(pct)))
    fill = int(round(p / 100 * width))
    return color_for(p) + "█" * fill + GRAY + "░" * (width - fill) + RESET


def fmt_dur(secs):
    secs = int(secs)
    if secs <= 0:
        return "now"
    days, rem = divmod(secs, 86400)
    hours, rem = divmod(rem, 3600)
    mins = rem // 60
    if days > 0:
        return "{}d{}h".format(days, hours) if hours else "{}d".format(days)
    if hours > 0:
        return "{}h{:02d}m".format(hours, mins) if mins else "{}h".format(hours)
    return "{}m".format(mins) if mins else "<1m"


def load_history():
    try:
        with open(os.path.join(STATE_DIR, HISTORY_FILE)) as f:
            h = json.load(f)
        return h if isinstance(h, list) else []
    except Exception:
        return []


def slope_per_hr(points):
    """Least-squares slope of pct vs. time, returned in %/hour."""
    n = len(points)
    if n < 2:
        return None
    mx = sum(p[0] for p in points) / n
    my = sum(p[1] for p in points) / n
    den = sum((x - mx) ** 2 for x, _ in points)
    if den == 0:
        return None
    num = sum((x - mx) * (y - my) for x, y in points)
    return num / den * 3600.0


def sum_session_delta(entries, key):
    """Sum, across sessions, each session's (max - min) of a cumulative counter over
    the entries — i.e. total burn of that counter during the interval. None if no data."""
    by_sid = {}
    for e in entries:
        v = e.get(key)
        if v is None:
            continue
        sid = e.get("sid")
        lo, hi = by_sid.get(sid, (v, v))
        by_sid[sid] = (min(lo, v), max(hi, v))
    if not by_sid:
        return None
    return sum(hi - lo for lo, hi in by_sid.values())


def analyze(hist, pct_key, reset_key, window_sec, now):
    latest = hist[-1]
    cur = latest.get(pct_key)
    cur_reset = latest.get(reset_key)
    if cur is None:
        return None

    # current reset segment (samples since the last reset)
    seg = [e for e in hist if e.get(reset_key) == cur_reset and e.get(pct_key) is not None]

    # recent window within the segment → "current" rate
    recent = [e for e in seg if (now - e.get("t", 0)) <= window_sec]
    if len(recent) < 2:
        recent = seg
    rate = slope_per_hr([(e["t"], e[pct_key]) for e in recent]) if len(recent) >= 2 else None
    span = (recent[-1]["t"] - recent[0]["t"]) if len(recent) >= 2 else 0

    # conversion anchored over the whole segment (more Δ% = steadier estimate)
    conv = None
    if len(seg) >= 2:
        d_pct = seg[-1][pct_key] - seg[0][pct_key]
        if d_pct > 0:
            t0, t1 = seg[0]["t"], seg[-1]["t"]
            win = [e for e in hist if t0 <= e.get("t", 0) <= t1]
            usd = sum_session_delta(win, "usd")
            tok = (sum_session_delta(win, "tin") or 0) + (sum_session_delta(win, "tout") or 0)
            conv = {
                "usd_per_pct": (usd / d_pct) if usd else None,
                "tok_per_pct": (tok / d_pct) if tok else None,
            }
    return {"cur": cur, "reset": cur_reset, "rate": rate, "span": span,
            "n": len(recent), "conv": conv}


def main():
    args = sys.argv[1:]
    window_min = DEFAULT_WINDOW_MIN
    if "--window" in args:
        try:
            window_min = float(args[args.index("--window") + 1])
        except Exception:
            pass

    hist = load_history()
    now = time.time()
    if len(hist) < 2:
        print("Not enough samples yet ({}). The status line logs ~1/min while sessions "
              "are active — check back in a few minutes.".format(len(hist)))
        return

    total_span = fmt_dur(hist[-1].get("t", now) - hist[0].get("t", now))
    print(BOLD + "Claude usage — burn rate" + RESET +
          DIM + "  ({} samples over {})".format(len(hist), total_span) + RESET)
    print()

    for label, pk, rk in (("5h", "h5", "h5r"), ("wk", "d7", "d7r")):
        a = analyze(hist, pk, rk, window_min * 60, now)
        if not a:
            continue
        cur, rate, reset = a["cur"], a["rate"], a["reset"]

        line = "  {b}{lbl}{r}  {bar}  {c}{cur:>3.0f}%{r}".format(
            b=BOLD, r=RESET, lbl=label, bar=bar(cur), c=color_for(cur), cur=cur)

        if cur >= 100:
            line += "  " + RED + "AT LIMIT" + RESET
        elif rate is not None and rate > 0.05:
            eta = (100 - cur) / rate * 3600.0
            line += "  {:+.1f}%/hr   ETA {}".format(rate, fmt_dur(eta))
        else:
            line += "  " + DIM + "~idle (no measurable burn)" + RESET

        if reset:
            ttr = reset - now
            line += DIM + "   resets in {}".format(fmt_dur(ttr)) + RESET
            if rate is not None and rate > 0.05 and cur < 100:
                eta = (100 - cur) / rate * 3600.0
                line += ("  " + RED + "→ hits cap first" + RESET) if eta < ttr \
                    else ("  " + GREEN + "→ resets first" + RESET)
        print(line)

        # headroom (cost-anchored, approximate)
        conv = a["conv"]
        if conv and conv.get("usd_per_pct"):
            rem = 100 - cur
            head = rem * conv["usd_per_pct"]
            extra = "      {d}headroom ~${h:.2f}   (${pp:.3f}/1%".format(
                d=DIM, h=head, pp=conv["usd_per_pct"])
            if conv.get("tok_per_pct"):
                extra += ", ≈{:,.0f} raw-tok/1%".format(conv["tok_per_pct"])
            print(extra + ")" + RESET)
        else:
            print("      " + DIM + "headroom: n/a — need more % movement to anchor a $/% estimate" + RESET)
        print()

    print(DIM + "  % and ETA are exact (account-wide). $/token are estimates — they assume your" + RESET)
    print(DIM + "  usage is mostly these sessions; claude.ai etc. moves % but isn't logged." + RESET)
    print(DIM + "  raw-tok counts include cache re-reads, so the $ figure is the steadier one." + RESET)


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print("burn: {}".format(e))

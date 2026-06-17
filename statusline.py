#!/usr/bin/env python3
"""
Claude Code status line: account-wide 5-hour + weekly usage-limit gauges, with an
inline burn-rate readout.

    5h ████████░░░░░░  19%  resets in 2h57m @ 8:50pm (Wed)  +18%/h
    wk ███░░░░░░░░░░░   8%  resets in 6d6h @ 12am (Wed)     +1.2%/h  cap 1d6h!

Per window: a full-width bar (green < 80%, amber 80–89%, red >= 90%, tracking the live
percentage so a reset returns it to green), a local-time reset readout, the current
burn rate in %/hour, and a red `cap <eta>!` warning that appears only when, at the
current rate, you would hit 100% BEFORE the window resets.

Data comes only from the `rate_limits` object Claude Code pipes on stdin (zero network,
zero auth, no Terms-of-Service surface). The limits are account-wide, so this reflects
your total across every session. Each render also appends a sample to
.state/usage-history.json (throttled ~1/min, flock-guarded, pruned) — the same history
this script reads to compute the burn rate, and that `burn.py` reads for $ headroom.
"""

import sys
import os
import json
import time
from datetime import datetime

try:
    import fcntl   # POSIX only; absent on Windows (history logging then runs lock-free)
except ImportError:
    fcntl = None

# ANSI
RESET = "\033[0m"
DIM = "\033[2m"
GRAY = "\033[90m"
GREEN = "\033[32m"
AMBER = "\033[38;5;214m"   # 256-colour amber (warning band)
RED = "\033[31m"

STATE_DIR = os.environ.get("CTT_STATE_DIR") or os.path.join(
    os.path.dirname(os.path.abspath(__file__)), ".state")
MIN_BAR = 8          # never shrink a bar below this
MAX_BAR = 150        # cap bar length on ultrawide screens (no clipping past it)
SAFE_MARGIN = 8      # columns kept free at the right edge (Claude Code insets ~5);
                     # line width is COLUMNS-8 until the bar hits MAX_BAR.
FALLBACK_COLS = 80

# Colour bands (percent of allowance used)
AMBER_AT = 80
RED_AT = 90

# Usage-history logging (also consumed by burn.py)
HISTORY_FILE = "usage-history.json"
LOCK_FILE = "usage-history.lock"
SAMPLE_INTERVAL = 60   # seconds; min gap between logged samples (global)
MAX_ENTRIES = 250      # prune to the last N samples (~4h at 1/min)
RATE_WINDOW_MIN = 120  # trailing window used for the inline burn rate


def color_for(pct):
    if pct is None:
        return GRAY
    if pct >= RED_AT:
        return RED
    if pct >= AMBER_AT:
        return AMBER
    return GREEN


def to_epoch(v):
    try:
        if isinstance(v, (int, float)):
            return float(v)
        if isinstance(v, str):
            s = v.strip()
            if not s:
                return None
            if s.replace(".", "", 1).isdigit():
                return float(s)
            return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
    except Exception:
        return None
    return None


def get_pct(window):
    if not isinstance(window, dict):
        return None
    for k in ("used_percentage", "utilization", "percent"):
        if window.get(k) is not None:
            try:
                return float(window[k])
            except Exception:
                pass
    return None


def get_reset_epoch(window):
    if not isinstance(window, dict):
        return None
    for k in ("resets_at", "reset_at", "resets"):
        if window.get(k) is not None:
            return to_epoch(window[k])
    return None


def fmt_duration(epoch):
    d = int(epoch - time.time())
    if d <= 0:
        return "now"
    days, rem = divmod(d, 86400)
    hours, rem = divmod(rem, 3600)
    mins = rem // 60
    if days > 0:
        return "{}d{}h".format(days, hours) if hours else "{}d".format(days)
    if hours > 0:
        return "{}h{:02d}m".format(hours, mins) if mins else "{}h".format(hours)
    return "{}m".format(mins) if mins else "<1m"


def fmt_clock(epoch):
    dt = datetime.fromtimestamp(epoch)   # naive == local timezone
    hour = dt.hour % 12 or 12
    ampm = "am" if dt.hour < 12 else "pm"
    return "{}:{:02d}{} ({})".format(hour, dt.minute, ampm, dt.strftime("%a"))


def reset_text(window):
    e = get_reset_epoch(window)
    if e is None:
        return "resets —"
    dur = fmt_duration(e)
    if dur == "now":
        return "resets now @ " + fmt_clock(e)
    return "resets in {} @ {}".format(dur, fmt_clock(e))


def pct_field(pct):
    txt = "--%" if pct is None else "{}%".format(int(round(pct)))
    return "{:>4}".format(txt)


def make_bar(pct, width):
    if pct is None:
        return GRAY + ("░" * width) + RESET
    p = max(0.0, min(100.0, float(pct)))
    filled = int(round(p / 100 * width))
    filled = max(0, min(width, filled))
    return color_for(p) + ("█" * filled) + GRAY + ("░" * (width - filled)) + RESET


# ---- burn rate (read from the same history we log) ---------------------------

def read_history():
    try:
        with open(os.path.join(STATE_DIR, HISTORY_FILE)) as f:
            h = json.load(f)
        return h if isinstance(h, list) else []
    except Exception:
        return []


def slope_per_hr(points):
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


def history_rate(hist, pct_key, reset_key, cur_reset, now):
    """%/hr over the recent window, restricted to the current reset segment."""
    try:
        seg = [e for e in hist if e.get(reset_key) == cur_reset and e.get(pct_key) is not None]
        recent = [e for e in seg if (now - e.get("t", 0)) <= RATE_WINDOW_MIN * 60]
        if len(recent) < 2:
            recent = seg
        if len(recent) < 2:
            return None
        return slope_per_hr([(e["t"], e[pct_key]) for e in recent])
    except Exception:
        return None


def fmt_rate(r):
    return ("%.1f" % r).rstrip("0").rstrip(".")


def burn_suffix(pct, reset_epoch, rate, now):
    """Return (plain, colored) inline burn readout, or ('','') if no rate yet."""
    if rate is None:
        return "", ""
    plain = "+{}%/h".format(fmt_rate(max(0.0, rate)))
    colored = DIM + plain + RESET
    try:
        if pct is not None and pct < 100 and rate > 0.5 and reset_epoch:
            eta = (100 - pct) / rate * 3600.0
            ttr = reset_epoch - now
            if 0 < eta < ttr:                       # hits cap before the window resets
                warn = "cap {}!".format(fmt_duration(now + eta))
                plain += "  " + warn
                colored += "  " + RED + warn + RESET
    except Exception:
        pass
    return plain, colored


def render_line(label, pct, bar_width, rt, suffix_colored):
    s = "{dim}{label}{r} {bar} {pc}{pf}{r}  {dim}{rt}{r}".format(
        dim=DIM, r=RESET, label=label, bar=make_bar(pct, bar_width),
        pc=color_for(pct), pf=pct_field(pct), rt=rt)
    if suffix_colored:
        s += "  " + suffix_colored
    return s


CTX_AMBER_TOK = 200_000   # context size where per-turn cost starts to bite
CTX_RED_TOK = 500_000     # ...and where it's expensive (re-read in full every turn)


def fmt_tokens(n):
    n = float(n)
    if n >= 1e6:
        return "{:.1f}M".format(n / 1e6)
    if n >= 1e3:
        return "{:.0f}k".format(n / 1e3)
    return str(int(n))


def ctx_color(abs_tok, pct):
    """Colour by the more severe of absolute size (cost) and % full (window risk)."""
    level = 0
    if abs_tok is not None:
        if abs_tok >= CTX_RED_TOK:
            level = 2
        elif abs_tok >= CTX_AMBER_TOK:
            level = 1
    if pct is not None:
        if pct >= 90:
            level = 2
        elif pct >= 70 and level < 1:
            level = 1
    return (GREEN, AMBER, RED)[level]


def session_header(data):
    """Compact 'Model · effort: level · ctx N% (size)' header; '' if all absent.
    The ctx value is coloured by absolute context size — the per-turn cost driver."""
    parts = []
    model = (data.get("model") or {}).get("display_name")
    effort = (data.get("effort") or {}).get("level")
    if model:
        parts.append(DIM + str(model) + RESET)
    if effort:
        parts.append(DIM + "effort: " + str(effort) + RESET)
    cw = data.get("context_window") or {}
    pct = cw.get("used_percentage")
    if pct is not None:
        abs_tok = cw.get("total_input_tokens")
        if abs_tok is None and cw.get("context_window_size"):
            abs_tok = pct / 100.0 * cw["context_window_size"]
        val = "{}%".format(int(round(pct)))
        if abs_tok:
            val += " ({})".format(fmt_tokens(abs_tok))
        parts.append(DIM + "ctx " + RESET + ctx_color(abs_tok, pct) + val + RESET)
    return (DIM + " · " + RESET).join(parts) if parts else ""


# ---- terminal width / state files --------------------------------------------

def term_width():
    col = os.environ.get("COLUMNS")
    if col and col.strip().isdigit():
        return int(col), "env"
    try:
        return os.get_terminal_size(sys.stdout.fileno()).columns, "tty"
    except Exception:
        return FALLBACK_COLS, "fallback"


def write_state(name, text):
    try:
        os.makedirs(STATE_DIR, exist_ok=True)
        with open(os.path.join(STATE_DIR, name), "w") as f:
            f.write(text)
    except Exception:
        pass


def log_sample(data, five, week):
    """Append one account-usage sample. Best-effort, throttled ~1/min globally, pruned,
    guarded by a NON-BLOCKING flock (skip-on-contention; auto-releases on death), and
    written via atomic replace so the file is never half-written. Where fcntl is absent
    (Windows) the lock is skipped — the atomic write still prevents a half-written file;
    the only cost is that two simultaneous renders could each drop a sample."""
    lock_fd = None
    try:
        os.makedirs(STATE_DIR, exist_ok=True)
        if fcntl is not None:
            lock_fd = os.open(os.path.join(STATE_DIR, LOCK_FILE), os.O_CREAT | os.O_RDWR, 0o644)
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError:
                return  # another session is writing this tick — skip

        path = os.path.join(STATE_DIR, HISTORY_FILE)
        hist = read_history()

        now = time.time()
        if hist and isinstance(hist[-1], dict) and (now - hist[-1].get("t", 0)) < SAMPLE_INTERVAL:
            return  # too soon since the last sample

        cost = data.get("cost") or {}
        cw = data.get("context_window") or {}
        hist.append({
            "t": round(now),
            "h5": get_pct(five),
            "d7": get_pct(week),
            "h5r": get_reset_epoch(five),
            "d7r": get_reset_epoch(week),
            "sid": (data.get("session_id") or "")[:12] or None,
            "usd": cost.get("total_cost_usd"),
            "tin": cw.get("total_input_tokens"),
            "tout": cw.get("total_output_tokens"),
        })
        if len(hist) > MAX_ENTRIES:
            hist = hist[-MAX_ENTRIES:]

        tmp = path + ".tmp"
        with open(tmp, "w") as f:
            json.dump(hist, f, separators=(",", ":"))
        os.replace(tmp, path)
    except Exception:
        pass
    finally:
        if lock_fd is not None:
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_UN)
            except Exception:
                pass
            try:
                os.close(lock_fd)
            except Exception:
                pass


def main():
    raw = sys.stdin.read()
    try:
        data = json.loads(raw) if raw.strip() else {}
    except Exception:
        data = {}

    write_state("last-input.json", raw)

    rl = data.get("rate_limits") or {}
    if not rl:
        sys.stdout.write(GRAY + "limits n/a (awaiting first API response)" + RESET)
        return

    five = rl.get("five_hour")
    week = rl.get("seven_day")
    now = time.time()
    hist = read_history()

    # live current values + historical burn rate per window
    five_pct, five_reset = get_pct(five), get_reset_epoch(five)
    week_pct, week_reset = get_pct(week), get_reset_epoch(week)
    five_sp, five_sc = burn_suffix(five_pct, five_reset,
                                   history_rate(hist, "h5", "h5r", five_reset, now), now)
    week_sp, week_sc = burn_suffix(week_pct, week_reset,
                                   history_rate(hist, "d7", "d7r", week_reset, now), now)

    rt5, rt7 = reset_text(five), reset_text(week)
    right5 = rt5 + ("  " + five_sp if five_sp else "")
    right7 = rt7 + ("  " + week_sp if week_sp else "")
    max_right = max(len(right5), len(right7))

    # equal, width-filling bars: overhead per line = label(2)+sp+sp+pct(4)+2sp = 10
    width, src = term_width()
    bar_width = max(MIN_BAR, min(MAX_BAR, width - SAFE_MARGIN - 10 - max_right))

    write_state("render-debug.txt",
                "COLUMNS_env={!r} width={} src={} margin={} max_bar={} max_right={} "
                "bar_width={} longest_line={}\n".format(
                    os.environ.get("COLUMNS"), width, src, SAFE_MARGIN, MAX_BAR,
                    max_right, bar_width, 10 + bar_width + max_right))

    lines = []
    header = session_header(data)
    if header:
        lines.append(header)
    lines.append(render_line("5h", five_pct, bar_width, rt5, five_sc))
    lines.append(render_line("wk", week_pct, bar_width, rt7, week_sc))
    out = "\n".join(lines)
    sys.stdout.write(out)
    sys.stdout.flush()

    log_sample(data, five, week)  # after flush, so it can never delay the line


if __name__ == "__main__":
    try:
        main()
    except Exception:
        pass

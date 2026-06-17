#!/usr/bin/env python3
"""End-to-end smoke test for statusline.py and burn.py.

Pipes sample payloads through both entry points against a throwaway CTT_STATE_DIR and
asserts they exit cleanly and emit the expected fields. Pure standard library, no
network. Run locally or in CI:

    python3 tests/smoke_test.py
"""

import json
import os
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
STATUSLINE = os.path.join(ROOT, "statusline.py")
BURN = os.path.join(ROOT, "burn.py")

failures = []


def check(name, cond, detail=""):
    print("  [{}] {}".format("ok " if cond else "FAIL", name)
          + ("" if cond or not detail else "  ({})".format(detail)))
    if not cond:
        failures.append(name)


def run(script, payload, state_dir, args=None):
    """Run a script with payload on stdin and a fixed CTT_STATE_DIR / width."""
    env = dict(os.environ, CTT_STATE_DIR=state_dir, COLUMNS="120")
    return subprocess.run(
        [sys.executable, script] + (args or []),
        input=payload, capture_output=True, text=True, env=env,
    )


def sample_payload(now):
    return json.dumps({
        "model": {"display_name": "Opus 4.8 (1M context)"},
        "effort": {"level": "high"},
        "context_window": {
            "used_percentage": 46, "total_input_tokens": 457000,
            "context_window_size": 1000000, "total_output_tokens": 12000,
        },
        "cost": {"total_cost_usd": 3.21},
        "session_id": "smoke-test-session",
        "rate_limits": {
            "five_hour": {"used_percentage": 25, "resets_at": int(now + 3000)},
            "seven_day": {"used_percentage": 9, "resets_at": int(now + 500000)},
        },
    })


def synthetic_history(now):
    """A few rising samples in one reset segment, enough for burn.py to fit a rate."""
    h5r, d7r = int(now + 3000), int(now + 500000)
    hist = []
    for frac in (0.0, 0.25, 0.5, 0.75, 1.0):
        hist.append({
            "t": int(now - 1800 + frac * 1800),
            "h5": 10 + frac * 15, "d7": 8 + frac * 1,
            "h5r": h5r, "d7r": d7r, "sid": "smoke12345678",
            "usd": 1.0 + frac * 2.2,
            "tin": 200000 + int(frac * 257000),
            "tout": 5000 + int(frac * 7000),
        })
    return hist


def main():
    now = time.time()
    print("smoke test — python {}".format(sys.version.split()[0]))

    # 1) statusline.py with a full payload renders the header + both bars and logs a sample.
    with tempfile.TemporaryDirectory() as d:
        p = run(STATUSLINE, sample_payload(now), d)
        check("statusline: exits 0", p.returncode == 0, p.stderr.strip())
        check("statusline: renders model header", "Opus 4.8 (1M context)" in p.stdout)
        check("statusline: renders 5h bar", "5h" in p.stdout)
        check("statusline: renders wk bar", "wk" in p.stdout)
        check("statusline: shows context fill", "ctx" in p.stdout)
        check("statusline: logged a history sample",
              os.path.exists(os.path.join(d, "usage-history.json")))

    # 2) No rate_limits → graceful "limits n/a", still exit 0.
    with tempfile.TemporaryDirectory() as d:
        p = run(STATUSLINE, json.dumps({"model": {"display_name": "Opus 4.8"}}), d)
        check("statusline: exits 0 without rate_limits", p.returncode == 0, p.stderr.strip())
        check("statusline: shows 'limits n/a'", "limits n/a" in p.stdout)

    # 3) Empty / garbage stdin must not crash.
    with tempfile.TemporaryDirectory() as d:
        p = run(STATUSLINE, "", d)
        check("statusline: exits 0 on empty stdin", p.returncode == 0, p.stderr.strip())
        p = run(STATUSLINE, "not json {{{", d)
        check("statusline: exits 0 on garbage stdin", p.returncode == 0, p.stderr.strip())

    # 4) burn.py with too little history reports gracefully.
    with tempfile.TemporaryDirectory() as d:
        with open(os.path.join(d, "usage-history.json"), "w") as f:
            json.dump([{"t": int(now), "h5": 10}], f)
        p = run(BURN, "", d)
        check("burn: exits 0 with sparse history", p.returncode == 0, p.stderr.strip())
        check("burn: reports 'not enough samples'", "Not enough samples" in p.stdout)

    # 5) burn.py with real history prints rate, ETA and headroom for both windows.
    with tempfile.TemporaryDirectory() as d:
        with open(os.path.join(d, "usage-history.json"), "w") as f:
            json.dump(synthetic_history(now), f)
        p = run(BURN, "", d)
        check("burn: exits 0 with history", p.returncode == 0, p.stderr.strip())
        check("burn: prints report header", "burn rate" in p.stdout)
        check("burn: covers 5h window", "5h" in p.stdout)
        check("burn: covers weekly window", "wk" in p.stdout)
        check("burn: computes headroom", "headroom" in p.stdout)
        # honors --window without error
        p = run(BURN, "", d, args=["--window", "60"])
        check("burn: accepts --window", p.returncode == 0, p.stderr.strip())

    print()
    if failures:
        print("FAILED ({}): {}".format(len(failures), ", ".join(failures)))
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

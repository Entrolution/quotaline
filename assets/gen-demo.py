#!/usr/bin/env python3
"""Render quotaline's real ANSI output to assets/demo.svg (a terminal-style image for the
README). Re-run after changing the renderer: python3 assets/gen-demo.py"""
import json, os, re, subprocess, sys, tempfile, html

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "quotaline")

# ANSI SGR → hex (matches quotaline's palette on a dark terminal)
COLORS = {"0": None, "2": "#6e7681", "1": "#e6edf3", "32": "#3fb950",
          "31": "#f85149", "90": "#6e7681", "38;5;214": "#e3a23c"}
FG_DEFAULT = "#d4d4d4"

def capture():
    t = tempfile.mkdtemp()
    os.makedirs(f"{t}/proj/memory")
    with open(f"{t}/proj/memory/MEMORY.md", "w") as f:
        f.write("# Project Memory\n\n" + "\n".join(
            f"- [Entry {i}](memory/e{i}.md) — a one-line hook describing this entry" for i in range(120)) + "\n")
    import time
    now = int(time.time()); h5r = now + 54*60; d7r = now + (6*24+4)*3600
    hist = [{"t": now-1800+int(f*1800), "h5": 10+f*15, "d7": 8+f*1, "h5r": h5r, "d7r": d7r,
             "sid": "demo12345678", "usd": 1.0+f*2.2, "tin": 200000+int(f*257000), "tout": 5000+int(f*7000)}
            for f in (0, .25, .5, .75, 1)]
    with open(f"{t}/usage-history.json", "w") as f:
        json.dump(hist, f)
    payload = {"model": {"display_name": "Opus 4.8 (1M context)"}, "effort": {"level": "max"},
               "context_window": {"used_percentage": 46, "total_input_tokens": 457000, "context_window_size": 1000000},
               "transcript_path": f"{t}/proj/sess.jsonl",
               "rate_limits": {"five_hour": {"used_percentage": 25, "resets_at": h5r},
                               "seven_day": {"used_percentage": 9, "resets_at": d7r}}}
    env = dict(os.environ, CTT_STATE_DIR=t, COLUMNS="92")
    out = subprocess.run([BIN], input=json.dumps(payload), capture_output=True, text=True, env=env).stdout
    return out.rstrip("\n")

def parse(line):
    """ANSI line → list of (text, color) segments."""
    segs, color, i = [], None, 0
    for m in re.finditer(r"\x1b\[([0-9;]*)m", line):
        if m.start() > i:
            segs.append((line[i:m.start()], color))
        code = m.group(1)
        color = FG_DEFAULT if code in ("", "0") else COLORS.get(code, color)
        i = m.end()
    if i < len(line):
        segs.append((line[i:], color))
    return segs

def main():
    lines = [parse(l) for l in capture().split("\n")]
    cw, lh, pad, top = 8.4, 22, 16, 34   # char width, line height, padding, titlebar height
    ncols = max(sum(len(t) for t, _ in segs) for segs in lines)
    W = round(ncols * cw + 2 * pad)
    H = round(top + len(lines) * lh + pad)
    svg = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" font-family="ui-monospace,SFMono-Regular,Menlo,Consolas,monospace" font-size="14">',
           f'<rect width="{W}" height="{H}" rx="8" fill="#0d1117"/>',
           f'<rect width="{W}" height="{top}" rx="8" fill="#161b22"/><rect y="{top-8}" width="{W}" height="8" fill="#161b22"/>',
           '<circle cx="18" cy="17" r="6" fill="#ff5f56"/><circle cx="38" cy="17" r="6" fill="#ffbd2e"/><circle cx="58" cy="17" r="6" fill="#27c93f"/>',
           f'<text x="{W/2}" y="21" fill="#6e7681" font-size="12" text-anchor="middle">quotaline</text>']
    for row, segs in enumerate(lines):
        y = top + pad + row * lh
        parts, col = [], 0
        for text, color in segs:
            x = pad + col * cw
            fill = color or FG_DEFAULT
            parts.append(f'<tspan x="{x:.1f}" textLength="{len(text)*cw:.1f}" lengthAdjust="spacingAndGlyphs" fill="{fill}">{html.escape(text)}</tspan>')
            col += len(text)
        svg.append(f'<text y="{y}" xml:space="preserve">{"".join(parts)}</text>')
    svg.append("</svg>")
    with open(os.path.join(ROOT, "assets", "demo.svg"), "w") as f:
        f.write("\n".join(svg) + "\n")
    print(f"wrote assets/demo.svg ({W}x{H}, {ncols} cols)")

if __name__ == "__main__":
    main()

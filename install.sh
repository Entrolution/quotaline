#!/usr/bin/env bash
#
# install.sh — wire (or remove) the quotaline status line in Claude Code.
#
# It finds your python3, resolves the absolute path to statusline.py (next to this
# script), and merges a `statusLine` block into ~/.claude/settings.json WITHOUT
# touching your other settings. Re-running is safe (idempotent); your settings file
# is backed up first.
#
#   ./install.sh              # install / update the status line
#   ./install.sh uninstall    # remove the statusLine block
#
# Overrides (env): CLAUDE_SETTINGS=path  CTT_REFRESH_INTERVAL=seconds
#
set -euo pipefail

# --- resolve the directory this script lives in (so statusline.py is found from any CWD)
SOURCE="${BASH_SOURCE[0]}"
while [ -h "$SOURCE" ]; do
  DIR="$(cd -P "$(dirname "$SOURCE")" >/dev/null 2>&1 && pwd)"
  SOURCE="$(readlink "$SOURCE")"
  [[ $SOURCE != /* ]] && SOURCE="$DIR/$SOURCE"
done
SCRIPT_DIR="$(cd -P "$(dirname "$SOURCE")" >/dev/null 2>&1 && pwd)"

STATUSLINE="$SCRIPT_DIR/statusline.py"
SETTINGS="${CLAUDE_SETTINGS:-$HOME/.claude/settings.json}"
REFRESH="${CTT_REFRESH_INTERVAL:-10}"
CMD="${1:-install}"

backup() {
  if [ -f "$SETTINGS" ]; then
    local bak="$SETTINGS.bak.$(date +%Y%m%d%H%M%S)"
    cp "$SETTINGS" "$bak"
    echo "backed up settings → $bak"
  fi
}

case "$CMD" in
  install)
    [ -f "$STATUSLINE" ] || { echo "error: statusline.py not found at $STATUSLINE" >&2; exit 1; }
    PYTHON="$(command -v python3 || true)"
    [ -n "$PYTHON" ] || { echo "error: python3 not found on PATH — install Python 3 first" >&2; exit 1; }

    mkdir -p "$(dirname "$SETTINGS")"
    backup

    # Merge with python3 itself (guaranteed present) so there's no jq dependency.
    "$PYTHON" - "$SETTINGS" "$PYTHON" "$STATUSLINE" "$REFRESH" <<'PY'
import json
import os
import shlex
import sys

settings, python, statusline, refresh = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])

data = {}
if os.path.exists(settings) and os.path.getsize(settings) > 0:
    with open(settings) as f:
        try:
            data = json.load(f)
        except Exception:
            sys.exit("error: %s exists but is not valid JSON — refusing to overwrite "
                     "(fix or move it, then re-run)" % settings)
    if not isinstance(data, dict):
        sys.exit("error: %s is not a JSON object — refusing to overwrite" % settings)

data["statusLine"] = {
    "type": "command",
    "command": "%s %s" % (shlex.quote(python), shlex.quote(statusline)),
    "refreshInterval": refresh,
}

tmp = settings + ".tmp"
with open(tmp, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
os.replace(tmp, settings)

print("statusLine installed → %s" % settings)
print("  command: %s" % data["statusLine"]["command"])
print("  refreshInterval: %ss" % refresh)
PY
    echo "done. Start a new Claude Code session (or wait ~${REFRESH}s) to see the status line."
    echo "note: limits show only on Pro/Max, after the session's first API response."
    ;;

  uninstall)
    [ -f "$SETTINGS" ] || { echo "nothing to do: $SETTINGS not found"; exit 0; }
    PYTHON="$(command -v python3 || true)"
    [ -n "$PYTHON" ] || { echo "error: python3 not found on PATH" >&2; exit 1; }
    backup
    "$PYTHON" - "$SETTINGS" <<'PY'
import json
import os
import sys

settings = sys.argv[1]
with open(settings) as f:
    data = json.load(f)
removed = data.pop("statusLine", None)

tmp = settings + ".tmp"
with open(tmp, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
os.replace(tmp, settings)

print("statusLine removed" if removed is not None else "no statusLine block was present")
PY
    ;;

  *)
    echo "usage: $0 [install|uninstall]" >&2
    exit 1
    ;;
esac

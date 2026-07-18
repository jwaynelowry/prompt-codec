#!/usr/bin/env bash
# Launch the prompt-codec proxy against Z.ai's GLM 5.2 (Anthropic endpoint).
#
# Loads Z_AI_API_KEY from ~/.claude/settings.local.json (the `env` object) into
# THIS process's environment without ever echoing the value, then execs the
# proxy so the key is inherited by the child and nothing else. Fails loudly if
# the file or key is missing.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SETTINGS="$HOME/.claude/settings.local.json"

if [[ ! -f "$SETTINGS" ]]; then
  echo "error: $SETTINGS not found — cannot load Z_AI_API_KEY" >&2
  exit 1
fi

# Extract env.Z_AI_API_KEY with python3's json parser (robust to quoting) and
# capture it into a shell var. The value is never printed; a missing key exits
# non-zero with a clear message (printed by python to stderr).
KEY="$(python3 - "$SETTINGS" <<'PY'
import json, sys
try:
    data = json.load(open(sys.argv[1]))
except Exception as e:
    sys.stderr.write(f"error: failed to parse settings.local.json: {e}\n")
    sys.exit(1)
key = (data.get("env") or {}).get("Z_AI_API_KEY")
if not key:
    sys.stderr.write("error: env.Z_AI_API_KEY missing from settings.local.json\n")
    sys.exit(1)
sys.stdout.write(key)
PY
)"
export Z_AI_API_KEY="$KEY"

BIN="$ROOT/target/release/prompt-codec"
if [[ ! -x "$BIN" ]]; then
  echo "Building release binary (first run)…" >&2
  (cd "$ROOT" && cargo build --release)
fi

echo "Dashboard: http://127.0.0.1:8788/dashboard"
exec "$BIN" proxy --config "$ROOT/config.glm.yaml"

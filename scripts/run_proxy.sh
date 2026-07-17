#!/usr/bin/env bash
# Launch Prompt Codec proxy (encode locally → paid upstream).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export PYTHONPATH="$ROOT${PYTHONPATH:+:$PYTHONPATH}"
exec python3 -m prompt_codec.cli proxy "$@"

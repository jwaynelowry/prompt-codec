#!/usr/bin/env bash
# Bootstrap prompt-codec on Apple Silicon for Hermes + Ollama/MLX.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PROFILE="${PROFILE:-recommended}" # recommended | light | rules
MODEL_RECOMMENDED="gemma4:e4b-mlx"
MODEL_LIGHT="qwen3.5:4b-mlx"

echo "=== prompt-codec mac setup (profile=$PROFILE) ==="

if ! command -v ollama >/dev/null 2>&1; then
  echo "Install Ollama first: https://ollama.com/download"
  exit 1
fi

if ! curl -sf --max-time 2 http://127.0.0.1:11434/api/tags >/dev/null; then
  echo "Starting Ollama…"
  if command -v brew >/dev/null 2>&1; then
    brew services start ollama 2>/dev/null || true
  fi
  (ollama serve >/tmp/ollama-serve-setup.log 2>&1 &) || true
  for i in $(seq 1 30); do
    curl -sf --max-time 1 http://127.0.0.1:11434/api/tags >/dev/null && break
    sleep 1
  done
fi

case "$PROFILE" in
  recommended)
    echo "Pulling $MODEL_RECOMMENDED (~8.8 GB)…"
    ollama pull "$MODEL_RECOMMENDED"
    MODEL="$MODEL_RECOMMENDED"
    MODE="hybrid"
    ;;
  light)
    echo "Pulling $MODEL_LIGHT (~4 GB)…"
    ollama pull "$MODEL_LIGHT"
    MODEL="$MODEL_LIGHT"
    MODE="hybrid"
    ;;
  rules)
    MODEL="$MODEL_RECOMMENDED"
    MODE="rules"
    echo "Rules-only profile — skipping model pull."
    ;;
  *)
    echo "Unknown PROFILE=$PROFILE (use recommended|light|rules)"
    exit 1
    ;;
esac

CFG_DST="${PROMPT_CODEC_CONFIG:-$HOME/.config/prompt-codec/config.yaml}"
mkdir -p "$(dirname "$CFG_DST")"
if [[ ! -f "$CFG_DST" ]]; then
  cp "$ROOT/config.example.yaml" "$CFG_DST"
  # Point at chosen model/mode with a light sed (example file is commented).
  if command -v python3 >/dev/null 2>&1; then
    MODEL="$MODEL" MODE="$MODE" CFG_DST="$CFG_DST" python3 - <<'PY'
import os, re
from pathlib import Path
p = Path(os.environ["CFG_DST"])
text = p.read_text()
model, mode = os.environ["MODEL"], os.environ["MODE"]
text = re.sub(r'(?m)^(\s*model:\s*).*$', rf'\1"{model}"', text, count=1)
text = re.sub(r'(?m)^(\s*mode:\s*).*$', rf'\1{mode}', text, count=1)
# Prefer Hermes OAuth proxy when present
if "upstream_base_url:" in text:
    text = re.sub(
        r'(?m)^(\s*upstream_base_url:\s*).*$',
        r'\1"http://127.0.0.1:8317/v1"',
        text,
        count=1,
    )
p.write_text(text)
print(f"wrote {p}")
PY
  else
    echo "Copied example config to $CFG_DST — edit local.model to $MODEL"
  fi
else
  echo "Config already exists: $CFG_DST (not overwritten)"
fi

if [[ -x "$ROOT/target/release/prompt-codec" ]]; then
  BIN="$ROOT/target/release/prompt-codec"
elif command -v prompt-codec >/dev/null 2>&1; then
  BIN="$(command -v prompt-codec)"
else
  echo "Building release binary…"
  cargo build --release
  BIN="$ROOT/target/release/prompt-codec"
fi

echo
echo "=== doctor ==="
"$BIN" doctor --config "$CFG_DST" || true

echo
echo "=== Hermes snippet (add to ~/.hermes/config.yaml) ==="
cat <<'YAML'
providers:
  prompt_codec:
    api: http://127.0.0.1:8787/v1
    name: prompt_codec
    api_key: ${X_API_KEY}
    transport: chat_completions
model:
  default: grok-4.5
  provider: custom:prompt_codec
  base_url: http://127.0.0.1:8787/v1
YAML

echo
echo "Start densify:  $BIN proxy --config $CFG_DST"
echo "Keep OAuth up:  hermes proxy start --provider xai   # :8317"
echo "Optional launchd: see contrib/ai.prompt-codec.proxy.plist"
echo "Done."

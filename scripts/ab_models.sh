#!/usr/bin/env bash
# Hybrid local-model A/B for Hermes-shaped traffic.
# Protocol: warm model, median of N timed runs, 15s encode budget (from config),
# report token savings + latency + truncation/degrade notes.
#
# Usage:
#   scripts/ab_models.sh
#   MODELS="qwen3.5:4b-mlx gemma4:e4b-mlx" RUNS=3 scripts/ab_models.sh
#   FIDELITY_ONLY=1 scripts/ab_models.sh   # skip corpus, run planted-fact probe only
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

BIN="${BIN:-./target/release/prompt-codec}"
RUNS="${RUNS:-3}"
MODELS="${MODELS:-qwen3.5:4b-mlx gemma4:e4b-mlx qwen3.5:9b-mlx lfm2.5:8b-a1b-q4_K_M}"
CORPUS_FILES=(
  tests/corpus/fluffy.txt
  tests/corpus/code_heavy.md
  tests/corpus/tool_dump.json
  tests/corpus/hermes_tool_turn.txt
)
CFG_TMP="$(mktemp -t prompt-codec-ab.XXXXXX.yaml)"
trap 'rm -f "$CFG_TMP"' EXIT

if [[ ! -x "$BIN" ]]; then
  echo "building release binary…"
  cargo build --release
fi

# Planted facts for fidelity probe (must all appear in compressed output).
FIDELITY_SRC="$(mktemp -t fidelity.XXXXXX.txt)"
cat >"$FIDELITY_SRC" <<'EOF'
Please help — thank you so much in advance, this is really important!

Refactor authentication in src/auth/session.py. There is also a helper in
src/auth/tokens.py but do not touch it.

Bug / exact error text:
  TokenRotationError: expected new jti, got reuse of abc123

Requirements (preserve every concrete value):
- Rotate refresh tokens on every use
- Invalidate old token family on reuse detection
- Redis key prefix: sess:
- TTL is 30 days
- Add unit tests with pytest
- Keep the public API stable
- Use existing logging helpers
- Framework: FastAPI on Python 3.11
- Stack frames mention session.py:214
- Family id example: fam-9f2e
- Endpoint path: /auth/refresh
- CI job name: test-auth

Please write clean production-ready code with comments. Thanks again!
EOF

FACTS=(
  "src/auth/session.py"
  "TokenRotationError: expected new jti, got reuse of abc123"
  "abc123"
  "sess:"
  "30 days"
  "pytest"
  "FastAPI"
  "Python 3.11"
  "session.py:214"
  "fam-9f2e"
  "/auth/refresh"
  "test-auth"
  "src/auth/tokens.py"
  "public API"
)

median_of() {
  # stdin: one float/int per line → stdout median
  sort -n | awk '
    { a[NR]=$1 }
    END {
      if (NR==0) { print "nan"; exit }
      if (NR%2) print a[(NR+1)/2]
      else print (a[NR/2]+a[NR/2+1])/2
    }'
}

write_cfg() {
  local model="$1"
  local cache_dir
  cache_dir="$(mktemp -d -t prompt-codec-ab-cache.XXXXXX)"
  # Isolated memory-only cache so prior SQLite hits don't fake latency/savings.
  cat >"$CFG_TMP" <<EOF
local:
  base_url: "http://127.0.0.1:11434/v1"
  api_key: "ollama"
  model: "$model"
  temperature: 0.1
  max_tokens: 2048
  reasoning_effort: "none"
encoder:
  mode: hybrid
  target_ratio: 0.45
  protect_system_under_chars: 800
  min_chars_to_compress: 400
  rules_enabled: true
  llm_scope: last_user
  llm_timeout_s: 15
cache:
  persist: false
  path: "$cache_dir/rewrites.sqlite3"
proxy:
  host: "127.0.0.1"
  port: 8787
  upstream_base_url: "https://api.x.ai/v1"
  upstream_api_key_env: "X_API_KEY"
  pass_client_auth: true
  log_stats: false
stats:
  usd_per_mtok_input: 3.0
EOF
}

unload_others() {
  local keep="$1"
  local name
  while IFS= read -r name; do
    [[ -z "$name" || "$name" == "$keep" ]] && continue
    echo "  unloading $name"
    curl -sS http://127.0.0.1:11434/api/generate \
      -d "{\"model\":\"$name\",\"keep_alive\":0}" >/dev/null || true
  done < <(curl -sS http://127.0.0.1:11434/api/ps 2>/dev/null \
    | python3 -c 'import sys,json
try:
  d=json.load(sys.stdin)
  for m in d.get("models",[]):
    print(m.get("name") or m.get("model") or "")
except Exception:
  pass')
}

warm_model() {
  local model="$1"
  local i elapsed
  echo "  warming ${model} (unload others, keep_alive 60m)..."
  unload_others "$model"
  # Cold MLX load can exceed 15s; retry until a short generate finishes quickly.
  for i in 1 2 3 4 5 6; do
    elapsed=$(
      python3 - <<PY
import json, time, urllib.request
t0=time.time()
req=urllib.request.Request(
  "http://127.0.0.1:11434/api/generate",
  data=json.dumps({
    "model": "$model",
    "prompt": "ping",
    "stream": False,
    "keep_alive": "60m",
    "options": {"num_predict": 4},
  }).encode(),
  headers={"Content-Type":"application/json"},
)
try:
  with urllib.request.urlopen(req, timeout=120) as r:
    r.read()
  print(round(time.time()-t0, 3))
except Exception as e:
  print("fail")
PY
    )
    echo "    warm try$i -> ${elapsed}s"
    if [[ "$elapsed" != "fail" ]] && python3 -c "import sys; sys.exit(0 if float('$elapsed') < 8 else 1)"; then
      return 0
    fi
  done
  echo "  warning: model still slow after warm attempts; A/B may hit 15s timeout"
}

encode_once() {
  # args: file → prints "before after seconds notes" via --json
  local file="$1"
  local start end elapsed json before after notes mode
  start=$(python3 -c 'import time; print(time.time())')
  json="$("$BIN" encode --mode hybrid --config "$CFG_TMP" --json -f "$file" 2>/dev/null)" || {
    end=$(python3 -c 'import time; print(time.time())')
    elapsed=$(python3 -c "print(round($end-$start, 3))")
    echo "0 0 $elapsed FAIL"
    return 0
  }
  end=$(python3 -c 'import time; print(time.time())')
  elapsed=$(python3 -c "print(round($end-$start, 3))")
  before=$(python3 -c "import json,sys; d=json.load(sys.stdin); print(d['stats']['before_tokens'])" <<<"$json")
  after=$(python3 -c "import json,sys; d=json.load(sys.stdin); print(d['stats']['after_tokens'])" <<<"$json")
  notes=$(python3 -c "import json,sys; d=json.load(sys.stdin); n=d.get('notes') or []; print(','.join(n) if isinstance(n,list) else n)" <<<"$json")
  echo "$before $after $elapsed $notes"
}

fidelity_score() {
  local out="$1"
  local hit=0
  local f
  for f in "${FACTS[@]}"; do
    if grep -Fqi -- "$f" <<<"$out"; then
      hit=$((hit + 1))
    fi
  done
  echo "$hit/${#FACTS[@]}"
}

echo "=== hybrid model A/B (runs=$RUNS) ==="
echo "binary: $BIN"
echo "models: $MODELS"
echo

RESULTS_MD="$(mktemp -t ab-results.XXXXXX.md)"
{
  echo "# Local model A/B (hybrid, Hermes traffic)"
  echo
  echo "Generated: $(date -u +%Y-%m-%dT%H:%MZ)"
  echo "Protocol: warm model, median of ${RUNS} timed runs, \`encoder.llm_timeout_s: 15\`."
  echo
} >"$RESULTS_MD"

if [[ "${FIDELITY_ONLY:-0}" != "1" ]]; then
  echo "| file | model | rules-ish before | after (median) | latency s (median) | notes |" >>"$RESULTS_MD"
  echo "|------|-------|------------------|----------------|--------------------|-------|" >>"$RESULTS_MD"

model_pulled() {
  local want="$1"
  curl -sS --max-time 5 http://127.0.0.1:11434/api/tags \
    | python3 -c 'import sys,json; want=sys.argv[1]; names={m.get("name") for m in json.load(sys.stdin).get("models",[])}; sys.exit(0 if want in names else 1)' "$want"
}

  for model in $MODELS; do
    echo "######## $model ########"
    if ! model_pulled "$model"; then
      echo "  SKIP — not pulled (missing from /api/tags)"
      echo "| *(all)* | \`$model\` | — | SKIP (not pulled) | — | — |" >>"$RESULTS_MD"
      continue
    fi
    write_cfg "$model"
    warm_model "$model"
    for file in "${CORPUS_FILES[@]}"; do
      echo "  -- $file"
      times=()
      afters=()
      befores=()
      note=""
      for ((i = 1; i <= RUNS; i++)); do
        read -r before after elapsed notes < <(encode_once "$file")
        befores+=("$before")
        afters+=("$after")
        times+=("$elapsed")
        note="$notes"
        echo "     run$i: before=$before after=$after ${elapsed}s notes=$notes"
      done
      med_after=$(printf '%s\n' "${afters[@]}" | median_of)
      med_time=$(printf '%s\n' "${times[@]}" | median_of)
      before0="${befores[0]}"
      echo "| \`$(basename "$file")\` | \`$model\` | $before0 | $med_after | $med_time | $note |" >>"$RESULTS_MD"
    done
  done
  echo >>"$RESULTS_MD"
fi

echo "### Fidelity probe (14 planted facts, 1 run each)" >>"$RESULTS_MD"
echo >>"$RESULTS_MD"
echo "| model | score | notes |" >>"$RESULTS_MD"
echo "|-------|-------|-------|" >>"$RESULTS_MD"

for model in $MODELS; do
  echo "######## fidelity $model ########"
  if ! model_pulled "$model"; then
    echo "| \`$model\` | SKIP | not pulled |" >>"$RESULTS_MD"
    continue
  fi
  write_cfg "$model"
  warm_model "$model"
  start=$(python3 -c 'import time; print(time.time())')
  out="$("$BIN" encode --mode hybrid --config "$CFG_TMP" -f "$FIDELITY_SRC" 2>/dev/null)" || out=""
  end=$(python3 -c 'import time; print(time.time())')
  elapsed=$(python3 -c "print(round($end-$start, 3))")
  if [[ -z "$out" ]]; then
    echo "| \`$model\` | 0/14 | FAIL/empty ${elapsed}s |" >>"$RESULTS_MD"
    echo "  FAIL empty (${elapsed}s)"
    continue
  fi
  score=$(fidelity_score "$out")
  echo "| \`$model\` | $score | ${elapsed}s |" >>"$RESULTS_MD"
  echo "  score=$score (${elapsed}s)"
done

echo
echo "=== wrote results ==="
cat "$RESULTS_MD"
OUT_DOC="$ROOT/docs/model-ab.md"
mkdir -p "$ROOT/docs"
cp "$RESULTS_MD" "$OUT_DOC"
echo "Copied to $OUT_DOC"
rm -f "$FIDELITY_SRC"

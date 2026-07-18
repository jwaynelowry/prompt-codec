#!/usr/bin/env bash
# Compare Rust v2 vs legacy Python rules compression on the golden corpus.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --quiet --release
for f in tests/corpus/*; do
  rust_json=$(./target/release/prompt-codec encode --mode rules --file "$f" --json)
  before=$(printf '%s' "$rust_json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["stats"]["before_tokens"])')
  rust_out=$(printf '%s' "$rust_json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["stats"]["after_tokens"])')
  py_out=$(cd legacy && PYTHONPATH=. python3 -m prompt_codec.cli encode --mode rules -f "../$f" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["stats"]["after_tokens"])')
  echo "$f  before=$before  rust_after=$rust_out  python_after=$py_out"
done

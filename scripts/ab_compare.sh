#!/usr/bin/env bash
# Compare Rust v2 vs legacy Python rules compression on the golden corpus.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --quiet --release
for f in tests/corpus/*; do
  rust_out=$(./target/release/prompt-codec encode --mode rules --file "$f" --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["stats"]["after_tokens"])')
  py_out=$(cd legacy && PYTHONPATH=. python3 -m prompt_codec.cli encode --mode rules -f "../$f" --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["stats"]["after_tokens"])')
  before=$(./target/release/prompt-codec encode --mode rules --file "$f" --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["stats"]["before_tokens"])')
  echo "$f  before=$before  rust_after=$rust_out  python_after=$py_out"
done

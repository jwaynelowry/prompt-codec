# Prompt Codec

Local **coder agent** that optimizes prompts on your Mac **before** they hit paid APIs, so you burn fewer tokens (and dollars). Rewritten in Rust (v2) — same job, fence-safe rules, byte-stable caching, and a proxy that never mangles a response.

```
You / Hermes / Cursor
        │
        ▼
┌───────────────────┐
│  Prompt Codec     │  ← free local rules + optional local LLM (Ollama / MLX / Exo)
│  ENCODE (compress)│
└─────────┬─────────┘
          │ fewer tokens
          ▼
   Paid API (xAI / OpenAI / …)
```

## What you get

| Piece | Role |
|-------|------|
| **Encoder (coder)** | Compress / densify prompts: rules + local model rewrite |
| **CLI** | One-shot `encode` / `demo` / `health` |
| **Proxy** | OpenAI-compatible server on `:8787` — drop-in base URL |

## Install (Rust)

```bash
cd ~/projects/prompt-codec
cargo build --release
# binary lands at target/release/prompt-codec
cargo install --path .   # optional: puts `prompt-codec` on your PATH
```

## Quick start (no local model needed)

```bash
cd ~/projects/prompt-codec
cargo run -- demo
cargo run -- encode --mode rules -f some_prompt.txt
cargo run -- health
# or, after `cargo build --release`:
./target/release/prompt-codec demo
```

To run the server (`cargo run -- proxy` — it blocks the terminal), see the [Proxy section](#proxy-route-any-openai-client-through-the-codec) below.

## Local model (stronger savings)

Ollama is already on this Mac. Pull a small/medium instruct model, then set `config.yaml`:

```bash
ollama pull qwen3.5:4b-mlx   # A/B-tested default; or gemma4:12b-mlx (higher fidelity, slower)
# edit config.yaml → local.model
prompt-codec health
prompt-codec encode --mode hybrid "long prompt here..."
```

Also works with:

- **MLX-LM server**: `mlx_lm.server --model mlx-community/Qwen3.6-27B-4bit` → `http://127.0.0.1:8080/v1`
- **Exo**: `http://127.0.0.1:52415/v1` (cluster)

## Proxy (route any OpenAI client through the codec)

```bash
export X_API_KEY=...          # or whatever proxy.upstream_api_key_env is
prompt-codec proxy
# → http://127.0.0.1:8787/v1
```

Point tools at the proxy:

```bash
# curl example
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $X_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"grok-4.5","messages":[{"role":"user","content":"…long prompt…"}]}'
```

### Hermes wiring

In `~/.hermes/config.yaml` (or a custom provider), add a provider that hits the proxy:

```yaml
providers:
  prompt_codec:
    api: http://127.0.0.1:8787/v1
    name: prompt_codec
    api_key: ${X_API_KEY}
    transport: chat_completions
```

Then set `model.base_url` / provider to `prompt_codec` when you want compressed sends.  
Keep a direct xAI provider for cases where you want zero encode latency.

### DROID / Factory custom model

Add to `~/.factory/settings.json` `customModels`:

```json
{
  "model": "grok-4.5",
  "displayName": "Grok via PromptCodec",
  "baseUrl": "http://127.0.0.1:8787/v1",
  "apiKey": "env:X_API_KEY",
  "provider": "generic-chat-completion-api",
  "maxOutputTokens": 16384
}
```

## Modes

| Mode | Needs local LLM? | Behavior |
|------|------------------|----------|
| `rules` | No | Fence-safe whitespace collapse, line dedupe, strip fluff |
| `local` | Yes | Full rewrite by local model toward `target_ratio` |
| `hybrid` | Preferred | Rules first, then local refine (best default) |

Encoder **never answers the task** — it only rewrites the prompt.  
If local encode fails, times out, or doesn't actually save tokens, it falls back to the rules output (or the original text, in `local` mode).

Note: `hybrid` stays the configured mode even when the local model isn't
available — requests then get the rules stage only (deterministic, still
saves tokens) and each skipped local call is logged. `prompt-codec health`
exits 1 and the proxy warns at startup when the configured model isn't
pulled, so this degradation is never silent.

## v2 behavior notes

A few things changed on purpose in the Rust rewrite — each one fixes a verified defect or gap in the legacy Python:

- **Fence-safe rules.** The `rules` pipeline segments text into prose vs. fenced code/JSON blocks first. Boilerplate stripping, line dedupe, and whitespace collapse only ever touch prose. Fenced content is byte-identical in the output, except that an *exact* whole-body duplicate fence (the same code block pasted twice) is replaced with a `[duplicate code block removed]` marker. The legacy Python `rules_compress` ran its passes over the whole text, fences included — see the A/B table below for what that costs in practice.
- **`last_user` LLM scope + cache = byte-stable history.** Only the most recent user message is eligible to *call* the local model on a cache miss, but every eligible message checks the cache first regardless of scope. Practically: re-sent conversation history compresses identically turn over turn (a prior turn's compressed text doesn't drift), which keeps the upstream provider's own prompt cache warm instead of invalidating it on every request.
- **15s hard LLM timeout.** `encoder.llm_timeout_s` (default 15) bounds every local-model call. A timeout, non-2xx, unparseable body, empty output, or a response cut off at `max_tokens` (`finish_reason: "length"`) all degrade to the rules output — never propagated as an error to the caller.
- **Verbatim error/streaming passthrough.** The proxy has exactly one forward path (streaming and non-streaming share it): upstream status, headers, and body bytes pass through unchanged. An upstream 429 or 500 reaches the client as that same 429 or 500, never silently reshaped into a 200; SSE chunks are streamed as received, never buffered or rewritten.
- **Host-header guard.** When bound to loopback, a request whose `Host` header isn't itself loopback is rejected with 403 — closes a DNS-rebinding hole where a malicious web page's `fetch()` could otherwise drive the local proxy and spend your upstream API key.

## Config

See `config.yaml` (your live config) / `config.example.yaml` (fully commented v2 reference).

| Key | Default | Notes |
|-----|---------|-------|
| `local.base_url` | `http://127.0.0.1:11434/v1` | OpenAI-compatible local server |
| `local.api_key` | `ollama` | most local servers ignore this |
| `local.model` | `qwen3.5:4b-mlx` | must match `ollama list` / your MLX tag |
| `local.reasoning_effort` | `none` | stops thinking models burning the output budget on hidden reasoning; `""` omits the field |
| `local.temperature` | `0.1` | |
| `local.max_tokens` | `2048` | ceiling; actual budget is sized per call |
| `encoder.mode` | `hybrid` | `rules` \| `local` \| `hybrid` |
| `encoder.target_ratio` | `0.45` | local-LLM target compression |
| `encoder.protect_system_under_chars` | `800` | leave short system prompts alone |
| `encoder.min_chars_to_compress` | `400` | skip tiny messages |
| `encoder.rules_enabled` | `true` | |
| `encoder.llm_scope` **(new)** | `last_user` | `last_user` \| `all` \| `none` — see above |
| `encoder.llm_timeout_s` **(new)** | `15` | hard per-call timeout, seconds |
| `encoder.list_trim_enabled` **(new)** | `false` | reserved, no-op today |
| `cache.max_entries` **(new)** | `4096` | LRU accepted-rewrite cache size |
| `proxy.host` / `proxy.port` | `127.0.0.1` / `8787` | |
| `proxy.upstream_base_url` | `https://api.x.ai/v1` | your paid provider |
| `proxy.upstream_api_key_env` | `X_API_KEY` | env var holding the key — never hardcode it |
| `proxy.pass_client_auth` | `true` | forward client's own Authorization upstream |
| `proxy.require_client_auth` **(new)** | `false` | 401 any request with no Authorization at all |
| `proxy.log_stats` | `true` | log before/after tokens + notes to stderr |
| `stats.usd_per_mtok_input` | `3.0` | rough $ savings display only |

**Superseded v1 keys** — accepted but ignored, each with a `warning:` line on stderr (never a hard error):

| v1 key | What happens now |
|--------|-------------------|
| `local.timeout_s` | ignored; use `encoder.llm_timeout_s` |
| `encoder.roles` | ignored; role policy is fixed in v2 (`user`/`system`/`tool` get rules, `assistant` passes through) |
| `stats.encoding` | ignored; tokenizer is fixed to `cl100k_base` |
| `decoder` (whole section) | ignored; the decoder feature doesn't exist in v2 |
| any other unknown key | ignored, with `unknown config key ignored: section.key` |

Run `prompt-codec health` (or any command) after editing your config — any leftover v1/unknown key shows up as a `warning:` on stderr.

## A/B: Rust v2 vs. legacy Python `rules` mode

`scripts/ab_compare.sh` runs both encoders over `tests/corpus/*` in `rules` mode and prints before/after token counts:

```
tests/corpus/code_heavy.md  before=981  rust_after=775  python_after=628
tests/corpus/fluffy.txt     before=255  rust_after=175  python_after=162
tests/corpus/tool_dump.json before=667  rust_after=667  python_after=489
```

Python looks like it compresses harder on `code_heavy.md` and `tool_dump.json` — it doesn't, it's *corrupting*. The legacy `rules_compress` runs boilerplate-stripping, line dedupe, and whitespace collapse over the raw text with no idea a code fence exists. On `tool_dump.json`, structural JSON lines (`},`, `{`, closing brackets at different nesting depths) look identical once trimmed, so the legacy line-deduper silently deletes "duplicate" braces — the Python output is no longer valid JSON (confirmed: `json.loads` fails on it), and it even strips the literal words "thank you" out of a JSON string value it should never have touched. The same global pass is what buys Python's extra savings on `code_heavy.md`'s fenced code — deleting lines that repeat *legitimately* inside real functions (`return None`, closing `}`), not fluff.

Rust's `rules` pipeline segments prose from fenced blocks first and never runs boilerplate/dedupe/whitespace transforms inside a fence — only an exact whole-body duplicate fence gets collapsed to a marker. That's why `tool_dump.json` comes back byte-identical (`rust_after == before`): the entire payload is one JSON fence, so there's nothing outside a fence to touch. On plain prose (`fluffy.txt`) the two are close (175 vs. 162 tokens); the small gap is Python's boilerplate regexes dropping the rest of a matched line where Rust's phrase-only patterns only remove the fluff phrase and keep the rest. Bottom line: v2 refuses transforms it can't prove are safe, even at the cost of a few percentage points of savings on code-heavy input.

## A/B: local models (hybrid mode, 2026-07-18)

Same corpus, hybrid mode, 15s budget, warm model, median of 3 timed runs:

| file | rules only | `gemma4:12b-mlx` | `qwen3.5:4b-mlx` |
|------|-----------|------------------|-------------------|
| fluffy.txt | 175 | 113 tok, 1.83s | **88 tok, 1.16s** |
| code_heavy.md | 775 | 377 tok, 5.78s | **261 tok, 4.22s** |
| tool_dump.json | 667 | failed (truncated), 5.15s wasted | **258 tok, 3.65s** |

qwen is the default: faster everywhere, deeper savings, and it handles large
fenced-JSON rewrites Gemma truncates on. **Fidelity** (14 planted facts,
N=10 runs, normalized matching): qwen keeps all 14 in 8/10 runs — its rare
misses drop a single redundant context value (a TTL bullet), never paths,
error text, or IDs. `gemma4:12b-mlx` keeps 14/14 in 10/10 runs with
byte-identical output — it stays pulled as the max-fidelity swap
(`local.model`, one line) when a prompt is sacred. Known, accepted trade-off;
re-run this A/B before ever changing the default.

## Safety / quality

- Paths, errors, IDs, and code evidence are preserved by construction (fence-safe rules) and by instruction (the local-LLM system prompt).
- Hybrid mode rejects local rewrites that don't actually save tokens vs. the post-rules baseline, or that got cut off at the model's `max_tokens` limit.
- Start with `rules` on production traffic, A/B quality, then enable `hybrid`.
- For agent tool dumps (logs, HTML, JSON), rules alone often cut **50–90%** — as long as they're outside a fence; see the A/B table above for what's inside one.

## Project layout

```
prompt-codec/
  Cargo.toml
  config.yaml            # your live config
  config.example.yaml    # fully commented v2 reference
  src/
    main.rs      # CLI entry point (parse + dispatch only)
    cli.rs       # input resolution + plain-text savings report
    codec.rs     # per-message/per-text compression orchestration
    rules.rs     # fence-aware deterministic compressor
    llm.rs       # local OpenAI-compatible client (Ollama/MLX/Exo)
    cache.rs     # bounded LRU cache of accepted rewrites
    proxy.rs     # axum OpenAI-compatible reverse proxy
    config.rs    # typed config + tolerant YAML loading
    stats.rs     # token/dollar savings math
    tokenizer.rs # cl100k_base token counting
  tests/
    corpus/          # golden fixtures: fluffy.txt, code_heavy.md, tool_dump.json
    golden_test.rs   # end-to-end corpus tests
    codec_test.rs, llm_test.rs, proxy_test.rs
  scripts/
    ab_compare.sh   # Rust vs. legacy Python A/B harness
  legacy/            # original Python implementation, kept for reference
                      # until it's deleted — not maintained, don't build on it
```

## License

MIT — see `LICENSE`.

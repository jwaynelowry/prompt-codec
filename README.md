# Prompt Codec

Local **densify proxy for [Hermes Agent](https://github.com/NousResearch/hermes-agent)**. It rewrites prompts on your Mac **before** they hit paid APIs, so Hermes burns fewer tokens (and dollars). Fence-safe rules, optional local LLM refine, byte-stable caching for upstream prompt caches, and a proxy that never mangles a response.

```
Hermes Agent
        │
        ▼
┌───────────────────┐
│  Prompt Codec     │  ← free local rules + optional local LLM (Ollama / MLX)
│  ENCODE (compress)│
└─────────┬─────────┘
          │ fewer tokens
          ▼
   Paid API (xAI / OpenAI / …)
```

This is **not** Hermes’s built-in context compression. Hermes compacting mid-session (`compression:` / `ContextCompressor`) summarizes history near the context limit. Prompt Codec densifies **every outbound request** before it leaves your machine. Use both — they stack.

## What you get

| Piece | Role |
|-------|------|
| **Encoder** | Compress / densify prompts: rules + local model rewrite |
| **CLI** | One-shot `encode` / `demo` / `health` |
| **Proxy** | OpenAI-compatible server on `:8787` — Hermes `providers:` target |

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

## Hermes wiring (primary path)

1. Start the proxy (keep a direct xAI / Portal provider for zero encode latency):

```bash
export X_API_KEY=...          # or whatever proxy.upstream_api_key_env is
prompt-codec proxy
# → http://127.0.0.1:8787/v1
```

2. Add a provider in `~/.hermes/config.yaml` that hits the proxy:

```yaml
providers:
  prompt_codec:
    api: http://127.0.0.1:8787/v1
    name: prompt_codec
    api_key: ${X_API_KEY}
    transport: chat_completions
```

3. Point Hermes at it when you want densified sends (`hermes model` or set `model.provider` / the matching custom provider). Keep your normal `xai-oauth` / Portal route for cases where you want zero encode latency.

**Why `llm_scope: last_user` matters for Hermes:** only the latest user turn calls the local model on a cache miss; resent history compresses byte-identically, so Anthropic / provider prompt-cache prefixes stay warm.

**Complementary, not a replacement:** leave Hermes `compression.enabled: true`. That layer still prunes long sessions. Prompt Codec only shrinks what you send on each API call.

## Local model (stronger savings)

```bash
ollama pull gemma4:e4b-mlx   # A/B-tested default for Hermes traffic
# edit config.yaml → local.model  (qwen3.5:4b-mlx = lighter ~4GB fallback)
prompt-codec health
prompt-codec encode --mode hybrid "long prompt here..."
```

Also works with MLX-LM (`http://127.0.0.1:8080/v1`) or Exo (`http://127.0.0.1:52415/v1`).

## Proxy details

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $X_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"grok-4.5","messages":[{"role":"user","content":"…long prompt…"}]}'
```

### Savings telemetry (`GET /health`)

`/health` reports a cumulative `totals` object across the proxy's lifetime,
persisted in the SQLite cache `meta` table (survives restarts; session-only
when `cache.persist: false`):

```json
"totals": {
  "requests": 1234,
  "before_tokens": 5678901,
  "after_tokens": 3456789,
  "saved_tokens": 2222112,
  "usd_saved_est": 6.666336,
  "upstream_cached_tokens": 890123,
  "responses_with_cache_info": 1100,
  "since": "2026-07-18T17:00:00Z"
}
```

Totals are **per proxy port**. `upstream_cached_tokens` is best-effort from the
last 16 KB of each response tail. Totals flush on every `/health` read, every
32 counted requests, and at graceful shutdown.

### Optional GLM demo dashboard

`config.glm.yaml` fronts Z.ai GLM on port **8788** with a local dashboard
(`/dashboard`). See `scripts/run_glm_demo.sh`. This is a development aid —
Hermes remains the supported production consumer.

## Modes

| Mode | Needs local LLM? | Behavior |
|------|------------------|----------|
| `rules` | No | Fence-safe whitespace collapse, line dedupe, strip fluff |
| `local` | Yes | Full rewrite by local model toward `target_ratio` |
| `hybrid` | Preferred | Rules first, then local refine (best default) |

Encoder **never answers the task** — it only rewrites the prompt.  
If local encode fails, times out, or doesn't actually save tokens, it falls back to the rules output (or the original text, in `local` mode).

`hybrid` stays the configured mode even when the local model isn't available —
requests then get the rules stage only. `prompt-codec health` exits 1 and the
proxy warns at startup when the configured model isn't pulled.

## v2 behavior notes

- **Fence-safe rules.** Prose vs. fenced code/JSON are segmented first; transforms never touch fence interiors (except exact whole-body duplicate fences → marker).
- **`last_user` LLM scope + cache = byte-stable history.** Keeps Hermes / upstream prompt caches warm.
- **15s hard LLM timeout.** Timeouts, empty output, or `finish_reason: length` all degrade to rules — never fail the Hermes turn.
- **Verbatim error/streaming passthrough.** Upstream 429/500 reach Hermes unchanged; SSE is not rewritten.
- **Host-header guard.** Non-loopback `Host` on a loopback bind → 403 (DNS-rebinding protection).

## Config

See `config.yaml` (your live config) / `config.example.yaml` (fully commented reference).

| Key | Default | Notes |
|-----|---------|-------|
| `local.base_url` | `http://127.0.0.1:11434/v1` | OpenAI-compatible local server |
| `local.api_key` | `ollama` | most local servers ignore this |
| `local.model` | `gemma4:e4b-mlx` | must match `ollama list` / your MLX tag |
| `local.reasoning_effort` | `none` | stops thinking models burning the output budget; `""` omits the field |
| `local.temperature` | `0.1` | |
| `local.max_tokens` | `2048` | ceiling; actual budget is sized per call |
| `local.keep_alive` | `60m` | Ollama residency pin (proxy, `local`/`hybrid` only); `"-1"` forever; `""` off |
| `encoder.mode` | `hybrid` | `rules` \| `local` \| `hybrid` |
| `encoder.target_ratio` | `0.45` | local-LLM target compression |
| `encoder.protect_system_under_chars` | `800` | leave short system prompts alone |
| `encoder.min_chars_to_compress` | `400` | skip tiny messages |
| `encoder.rules_enabled` | `true` | |
| `encoder.llm_scope` | `last_user` | `last_user` \| `all` \| `none` |
| `encoder.llm_timeout_s` | `15` | hard per-call timeout, seconds |
| `encoder.list_trim_enabled` | `false` | reserved, no-op today |
| `cache.max_entries` | `4096` | in-memory LRU size |
| `cache.persist` | `true` | durable SQLite tier |
| `cache.path` | platform cache dir | override SQLite location |
| `cache.max_disk_entries` | `100000` | disk-tier prune threshold |
| `proxy.host` / `proxy.port` | `127.0.0.1` / `8787` | |
| `proxy.upstream_base_url` | `https://api.x.ai/v1` | your paid provider |
| `proxy.upstream_api_key_env` | `X_API_KEY` | env var — never hardcode |
| `proxy.upstream_auth_style` | `bearer` | `bearer` \| `x_api_key` |
| `proxy.pass_client_auth` | `true` | forward client's auth upstream |
| `proxy.require_client_auth` | `false` | 401 when no Authorization |
| `proxy.log_stats` | `true` | log before/after tokens to stderr |
| `stats.usd_per_mtok_input` | `3.0` | rough $ savings display only |

**Superseded v1 keys** are ignored with a `warning:` on stderr. Run `prompt-codec health` after editing config.

## A/B: Rust v2 vs. legacy Python `rules` mode

`scripts/ab_compare.sh` runs both encoders over `tests/corpus/*` in `rules` mode:

```
tests/corpus/code_heavy.md  before=981  rust_after=775  python_after=628
tests/corpus/fluffy.txt     before=255  rust_after=175  python_after=162
tests/corpus/tool_dump.json before=667  rust_after=667  python_after=489
```

Python’s extra savings on fenced JSON/code come from corrupting fence interiors. Rust refuses unsafe transforms. See commit history / `scripts/ab_compare.sh` for the full analysis.

## A/B: local models (hybrid mode)

Corpus + Hermes-shaped fixtures, hybrid mode, 15s budget, warm model. Latest
shortlist results live in [`docs/model-ab.md`](docs/model-ab.md). Default is
`gemma4:e4b-mlx` (beats rules on every corpus file, no truncations, 14/14
fidelity). `qwen3.5:4b-mlx` remains a lighter fallback.

## Safety / quality

- Paths, errors, IDs, and code evidence are preserved by construction (fence-safe rules) and by instruction (local-LLM system prompt).
- Hybrid rejects rewrites that don’t save tokens vs. post-rules baseline, or that truncate at `max_tokens`.
- Start with `rules` on Hermes traffic, A/B quality, then enable `hybrid`.

## Project layout

```
prompt-codec/
  Cargo.toml
  config.yaml            # your live config (Hermes → this proxy)
  config.example.yaml    # fully commented reference
  docs/
    model-ab.md          # local-model A/B results
    wren.md              # parked purpose-built compressor note
  src/                   # Rust CLI + proxy
  tests/corpus/          # golden + Hermes-shaped fixtures
  scripts/
    ab_compare.sh        # Rust vs. legacy Python rules A/B
    ab_models.sh         # hybrid local-model shortlist A/B
  legacy/                # original Python (reference only)
```

## License

MIT — see `LICENSE`.

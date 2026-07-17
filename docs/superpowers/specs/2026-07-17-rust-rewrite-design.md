# prompt-codec v2 — Rust rewrite design

Date: 2026-07-17
Status: approved by Wayne (plain-language summary); technical decisions delegated to Claude
Supersedes: Python prompt-codec v0.1.0 (moves to `legacy/`)

## Problem

prompt-codec sits between local coding tools (Hermes, Cursor, DROID) and paid
OpenAI-compatible APIs (default upstream: xAI), compressing prompts to cut token
cost. A multi-agent review of the Python implementation found defects that
undermine both goals the user cares about — speed and cost:

1. The rules compressor corrupts code: it collapses leading indentation and
   deletes repeated lines (`}`, `return`) everywhere, including inside fenced
   code blocks and JSON tool output. Corrupted prompts cause failed agent runs
   and paid retries.
2. The proxy makes synchronous local-LLM calls (up to 120 s timeout) inside
   async handlers, freezing all concurrent traffic.
3. Every request re-compresses the entire conversation history through the
   local LLM: serial calls add seconds-to-minutes per turn, and nondeterministic
   rewrites of the history defeat upstream provider prompt-cache discounts
   (often 50–90% off repeated prefixes) — plausibly costing more than the
   compression saves.
4. Compression stats are injected into the request body sent upstream
   (`metadata` key), which strict providers can reject; the stats never reach
   the client despite the code comment claiming so.
5. The streaming path discards upstream status codes and headers, so a 401/429
   surfaces to clients as a 200 SSE stream.
6. The decoder feature is dead code, the shipped default model tag
   (`gemma4:12b-mlx`) does not exist, and config resolution breaks for
   non-editable installs.

## Goals

- **Lower paid-API cost**: safe compression + deterministic, cached handling of
  history so upstream prompt-cache discounts stay warm.
- **Faster requests**: no event-loop blocking, parallel request handling, local
  LLM consulted only where it pays for itself, instant-start static binary.
- **Never corrupt a prompt**: fenced code and structured tool output pass
  through byte-identical or provably-safe transformed (JSON minify; exact
  whole-duplicate fences replaced by a marker).
- Drop-in replacement: same port (8787), same `/v1` paths, same YAML config
  shape — existing client configs keep working.

## Non-goals

- Decoder (expanding terse replies) — dead code in v1, dropped.
- Multi-user auth, TLS, public deployment (localhost single-user tool).
- Anthropic-native or other non-OpenAI-compatible upstream protocols.
- Windows support (target: macOS; Linux expected to work, untested).

## Architecture

Single Rust crate `prompt-codec` (lib + thin bin). Stack: tokio, axum,
reqwest (streaming), serde/serde_json/serde_yaml, tiktoken-rs (cl100k_base,
loaded once), moka (LRU cache), clap, tracing.

Binary subcommands: `proxy`, `encode`, `demo`, `health`.

```
src/
  main.rs        clap dispatch only
  lib.rs         module exports
  rules.rs       fence-aware deterministic compressor (pure functions)
  tokenizer.rs   token counting; char/4 fallback if BPE load fails
  codec.rs       orchestration: rules → cache → LLM pass → savings guard
  llm.rs         async OpenAI-compatible client for Ollama/MLX
  cache.rs       SHA-256(content, ratio, model) → compressed text, moka LRU
  proxy.rs       axum app + upstream forwarding
  config.rs      YAML load, lookup order, warnings
  stats.rs       before/after tokens, USD estimate
```

Data flow (proxy): client request → parse `messages` → `codec.encode_messages`
→ forward upstream (client auth passthrough or env key) → stream response back
verbatim with upstream status + headers preserved → stats to logs + response
headers.

## Component behavior

### rules — fence-aware deterministic compressor

The input is segmented into **prose** and **fenced blocks** (``` fences).
Fenced content is never modified, with one exception: an exact duplicate of an
earlier fence body (full-body match) is replaced by a one-line
`[duplicate code block removed]` marker fence.

Prose-only transforms, in order:

1. Normalize line endings (CRLF/CR → LF).
2. Strip boilerplate phrases (curated, anchored regexes: "please …",
   "thank you …", "as an AI …", "write clean code / follow best practices"
   filler lines). Conservative: whole-phrase matches only, with word
   boundaries and terminal-punctuation anchors so meaning-bearing uses
   survive — "write a thank you email", "as an aid", and requirements
   following a filler phrase ("follow best practices and use bcrypt") are
   kept. Patterns never consume the rest of a line beyond the phrase itself.
3. Dedupe identical non-blank prose lines (stripped-key match) — only lines
   ≥ 12 chars, to spare short legitimate repeats ("- yes", "OK").
4. Collapse 3+ blank lines to one; trim trailing whitespace.
5. Collapse interior runs of 2+ spaces/tabs to one space, **preserving all
   leading indentation**, and skipping lines that contain a backtick
   (inline-code safety).

Long-list trimming (v1's `compress_long_lists`) is **off by default**
(`encoder.list_trim_enabled: false`) — it deletes data from tool output.

Invariants (unit + property tested):
- All fenced code is byte-identical after compression (except whole-duplicate
  removal).
- Idempotent: `rules(rules(x)) == rules(x)`.
- Never returns empty output for non-empty input.

### codec — compression policy

Per message role:
- `user`, `system`: full rules pipeline. System messages shorter than
  `protect_system_under_chars` (default 800) are untouched.
- `tool`: **structure-safe path only** — if content parses as JSON, minify it
  (serde_json compact); otherwise whitespace normalization only. Never
  boilerplate-stripped, deduped, or LLM-rewritten.
- `assistant`: untouched.
- Content that is an array of parts: transform only `{"type":"text"}` parts;
  all other fields of every message (`tool_calls`, `tool_call_id`, `name`, …)
  pass through untouched.

LLM pass (mode `hybrid` or `local`):
- Scope: `encoder.llm_scope: last_user` (default) — only the final `user`
  message is sent to the local model. `all` and `none` available. History
  messages get rules **plus a cache lookup**: a rewrite accepted on an earlier
  turn keeps its exact bytes on every later turn (for the life of the cache
  entry), so compression savings persist and the upstream prompt-cache prefix
  stays stable. On cache eviction or restart a history message falls back to
  its rules-only form — a one-time prefix divergence, deterministic thereafter.
- Eligibility: post-rules content ≥ `min_chars_to_compress` (default 400).
- Guard: accept the rewrite only if its token count is **strictly lower than
  the post-rules token count** (v1 compared against the pre-rules original,
  accepting rewrites worse than the free rules output) and non-trivial
  (> 20 chars). Otherwise keep the rules output; never fail the request.
- Timeout: `encoder.llm_timeout_s` (default **15 s**, was 120) → on timeout or
  any LLM error, fall back to rules output and note it in logs.
- Truncation guard: the request sizes `max_tokens` to the job
  (≈ `target_ratio × input_tokens × 1.5`, clamped to `[256, local.max_tokens]`)
  and any response with `finish_reason == "length"` is treated as an error —
  a truncated rewrite (which always "saves tokens") is never accepted.
  (v1 silently forwarded rewrites cut off mid-sentence at max_tokens=2048.)
- Log hygiene: LLM error notes truncate any response body to 200 chars;
  prompt content is never logged in full.

### cache

moka LRU (default 4096 entries, configurable), key =
SHA-256(content ‖ target_ratio ‖ model), value = accepted LLM rewrite.
Only LLM results are cached (rules are microsecond-cheap and deterministic).
Effect: a conversation resent turn after turn does zero repeat LLM work, and
identical input always yields identical compressed output.

### proxy — HTTP semantics

- `POST /v1/chat/completions`: parse body as `serde_json::Value`; compress
  `messages` per policy; **no other body mutation** (v1's `metadata` injection
  removed). Forward; stream response bytes verbatim. Upstream status code and
  headers are propagated on both streaming and non-streaming paths, so client
  error handling and retry logic see the truth. Adds response headers
  `x-prompt-codec-before`, `x-prompt-codec-after`, `x-prompt-codec-saved-pct`.
- `POST /v1/completions`: the string `prompt` field gets the `user`-role
  treatment (full rules pipeline; LLM-eligible as the "last user" content).
- Catch-all `/v1/*`: forward method, path, **query params**, and raw body
  bytes untouched (v1 dropped non-JSON bodies and POST query params); stream
  response verbatim.
- `GET /health`: config summary (including which config file was loaded),
  cache stats, local-LLM reachability probe with a 3 s timeout (`ok` requires
  status < 400; includes `model_present` — whether the configured model
  appears in the local `/models` listing — informational, may be null);
  never blocks other traffic.
- DNS-rebinding guard: when bound to loopback, requests whose `Host` header
  is not `localhost`/`127.0.0.1`/`[::1]` (any port) are rejected with 403 —
  a malicious web page can otherwise script requests to the local proxy and
  spend the user's upstream key.
- Auth: if the client sends `Authorization` and `pass_client_auth: true`
  (default), pass it through; else sign with the env var named by
  `upstream_api_key_env`. New `proxy.require_client_auth` (default `false`,
  documented: anything that can reach the port spends your key — bind stays
  `127.0.0.1`).
- Upstream unreachable / timeout → `502` with an OpenAI-shaped error body
  `{"error": {"message": …, "type": "upstream_error"}}`.
- All encode work runs in async context off the request path's critical
  blocking operations (tiktoken counting is CPU-cheap at these sizes;
  LLM calls are async reqwest with timeout).

### config

Same YAML shape as v1 minus `decoder`, plus new keys
(`encoder.llm_scope`, `encoder.llm_timeout_s`, `encoder.list_trim_enabled`,
`cache.max_entries`, `proxy.require_client_auth`).

Lookup order: `--config` flag → `$PROMPT_CODEC_CONFIG` → `./config.yaml` →
`~/.config/prompt-codec/config.yaml` → built-in defaults. The chosen source is
logged at startup. Unknown keys produce a **warning** naming the key (not a
hard error — friendlier for hand-edited configs; a stale `decoder:` block
warns and is ignored).

Superseded v1 keys: `encoder.llm_timeout_s` alone governs the local-LLM
request timeout — a present `local.timeout_s` is accepted-but-ignored with a
warning (honoring it would silently resurrect v1's 120 s stalls for drop-in
configs). `encoder.roles` is likewise ignored with a warning: per-role policy
is fixed in v2 (user/system compressed, tool structure-safe, assistant
untouched).

Default local model: `gemma3:4b` (v1 shipped the nonexistent `gemma4:12b-mlx`).

### CLI

- `prompt-codec proxy [--host --port --config]`
- `prompt-codec encode [TEXT] [--file] [--mode rules|local|hybrid] [--json]`
  (stdin supported) — prints compressed text + savings table.
- `prompt-codec demo` — rules-only demo on a bundled verbose sample.
- `prompt-codec health` — config source, local LLM probe, exit code 0/1.

## Error handling principles

- Compression must never break a request: any rules/LLM/cache failure degrades
  to passing the original content through, with a logged note.
- Upstream errors are the client's business: propagate status, headers, and
  body faithfully.
- Local-LLM outages degrade hybrid → rules silently (logged), matching v1's
  intent but without v1's 120 s stall.

## Testing

- **Unit**: every rules transform; config lookup order; guard logic; cache
  keying.
- **Property**: fenced code byte-identity; idempotence; non-empty output
  (proptest).
- **Golden corpus**: realistic prompts (code-heavy Python/JS, JSON tool dumps,
  fluffy prose) with snapshot outputs; asserts corruption-free and ≥ target
  savings on the fluffy set.
- **Proxy integration** (wiremock upstream): status/header passthrough incl.
  401 and 429, SSE chunk fidelity, auth pass-through vs env-key, catch-all raw
  body + query params, 502 shape when upstream is down.
- **Cache behavior**: identical conversation resent → zero additional LLM
  calls (counting mock), identical output bytes.
- **A/B harness**: script running the golden corpus through Rust v2 and
  `legacy/` Python, comparing savings and corruption before cutover.

## Acceptance criteria

1. `cargo test` green; `cargo clippy` clean.
2. Code-heavy prompt: all fenced blocks byte-identical through `encode` and
   the proxy.
3. Same conversation sent twice through the proxy: identical compressed
   history bytes, zero repeat local-LLM calls.
4. Upstream 401 surfaces to the client as 401 (streaming and non-streaming).
5. Rules-only proxy overhead < 5 ms p50 on a 10 KB prompt (local measurement).
6. Demo shows token savings on the bundled sample; savings stats appear in
   logs and `x-prompt-codec-*` response headers.

## Migration plan

1. Move Python package + tests to `legacy/` (already committed as baseline).
2. Scaffold crate; implement rules → codec/cache → llm → proxy → CLI.
3. Run A/B harness against legacy on the golden corpus.
4. User keeps existing client configs; nothing changes for Hermes/DROID
   (same URL). Delete `legacy/` when satisfied.

## Review findings → v2 fixes (traceability)

| Python defect (verified by review) | v2 resolution |
|---|---|
| Indentation collapse + line dedupe corrupt code/JSON | Fence-aware segmentation; prose-only transforms; tool JSON minify |
| Sync LLM calls block the event loop | Fully async reqwest with 15 s timeout |
| Whole history re-compressed every turn; defeats provider prompt cache | `llm_scope: last_user` + content-hash cache; deterministic rules |
| `metadata` stats injected into upstream body | Removed; stats via headers + logs |
| Streaming drops upstream status/headers | Verbatim passthrough with status/header propagation |
| Decoder dead code | Dropped |
| Nonexistent default model `gemma4:12b-mlx` | `gemma3:4b` |
| Config path breaks when pip-installed; silent fallback | Explicit lookup order, logged source, unknown-key warnings |
| Hybrid guard compares vs pre-rules tokens | Compares vs post-rules tokens |
| Unauthenticated LAN callers spend the user's key | Documented; optional `require_client_auth`; default bind 127.0.0.1 |
| Catch-all drops non-JSON bodies and POST query params | Raw byte + query passthrough |
| Truncated LLM output (finish_reason=length) silently accepted | Sized max_tokens + finish_reason guard |
| Boilerplate regexes eat meaning-bearing prose | Boundary/punctuation-anchored, phrase-only patterns |
| DNS rebinding: web pages can drive the local proxy | Host-header guard (403 on non-loopback Host) |

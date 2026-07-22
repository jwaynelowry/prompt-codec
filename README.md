# Prompt Codec

Local **densify proxy for [Hermes Agent](https://github.com/NousResearch/hermes-agent)**. It rewrites prompts on your Mac **before** they hit paid APIs, so Hermes burns fewer tokens (and dollars). Fence-safe rules, optional local LLM refine, byte-stable caching for upstream prompt caches, and a proxy that never mangles a response.

```
Hermes Agent  →  Prompt Codec (:8787)  →  Paid API (xAI / OpenAI / …)
                      │
                      └─ Ollama / MLX (gemma4:e4b-mlx)
```

This is **not** Hermes’s built-in context compression. Hermes compacting mid-session (`compression:` / `ContextCompressor`) summarizes history near the context limit. Prompt Codec densifies **every outbound request** before it leaves your machine. **Use both — they stack.**

> Disk that matters is the **local model** (~4–9 GB), not the ~15 MB binary.

## Install (60 seconds)

**Recommended (Apple Silicon + Ollama):**

```bash
# 1) Binary
cargo install --git https://github.com/jwaynelowry/prompt-codec
# or: download the macOS arm64 .tar.gz from GitHub Releases

# 2) Local encoder model (~8.8 GB)
ollama pull gemma4:e4b-mlx

# 3) Config + doctor
git clone https://github.com/jwaynelowry/prompt-codec.git   # for scripts/example config
cd prompt-codec && PROFILE=recommended ./scripts/setup-mac.sh

# 4) Run (awaits model pin before serving)
prompt-codec proxy --config ~/.config/prompt-codec/config.yaml
# → http://127.0.0.1:8787/v1
```

**Light profile** (~4 GB model): `PROFILE=light ./scripts/setup-mac.sh`  
**Rules-only** (no model): `PROFILE=rules ./scripts/setup-mac.sh` then `encoder.mode: rules`

```bash
prompt-codec doctor    # local model + upstream readiness
prompt-codec health
```

## Hermes wiring

1. Keep Hermes OAuth (or your paid key) working. On this machine the densify
   upstream is often the Hermes OAuth proxy:

```yaml
# prompt-codec config.yaml
proxy:
  upstream_base_url: "http://127.0.0.1:8317/v1"   # hermes proxy start --provider xai
```

2. Add a provider and point the default model at it:

```yaml
# ~/.hermes/config.yaml
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
```

3. Restart the Hermes gateway. Keep a direct `xai-oauth` fallback for zero encode latency.

**Why `llm_scope: last_user`:** only the latest user turn calls the local model on a cache miss; resent history stays byte-identical so upstream prompt-cache prefixes stay warm.

Always-on proxy (Telegram): copy [`contrib/ai.prompt-codec.proxy.plist`](contrib/ai.prompt-codec.proxy.plist) into `~/Library/LaunchAgents/`, edit paths, `launchctl load` it.

Upstream Hermes PRs: [docs recipe #68835](https://github.com/NousResearch/hermes-agent/pull/68835), [provider plugin #69099](https://github.com/NousResearch/hermes-agent/pull/69099).

## What you get

| Piece | Role |
|-------|------|
| **Encoder** | Compress / densify prompts: rules + local model rewrite |
| **CLI** | `encode` / `demo` / `health` / `doctor` |
| **Proxy** | OpenAI-compatible server on `:8787` — Hermes `providers:` target |

## Local model

Default: **`gemma4:e4b-mlx`** (A/B winner — see [`docs/model-ab.md`](docs/model-ab.md)).

| Profile | Model | When |
|---------|-------|------|
| Recommended | `gemma4:e4b-mlx` (~8.8 GB) | Hermes traffic, best reliability under 15s |
| Light | `qwen3.5:4b-mlx` (~4 GB) | Smaller RAM/disk; may timeout on heavy tool dumps |
| Rules | none | Zero LLM latency; smaller savings |

Also works with MLX-LM (`http://127.0.0.1:8080/v1`) or Exo (`http://127.0.0.1:52415/v1`). `doctor` probes common bases if Ollama is down.

### Fine-tuning?

**Not yet.** e4b clears fidelity (14/14) and beats rules without truncations when warm. Remaining misses are operational (cold load, timeout, upstream auth). Revisit LoRA/fine-tune only after 100+ real Hermes turns with a recurring fact-drop class that prompt fixes cannot stop. See [`docs/model-ab.md`](docs/model-ab.md).

## Proxy details

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $X_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"grok-4.5","messages":[{"role":"user","content":"…long prompt…"}]}'
```

### Savings telemetry (`GET /health`)

`/health` reports cumulative `totals` (SQLite-persisted when `cache.persist: true`):

```json
"totals": {
  "requests": 42,
  "before_tokens": 120000,
  "after_tokens": 48000,
  "saved_tokens": 72000,
  "usd_saved_est": 0.216
}
```

## Config

See `config.yaml` / `config.example.yaml`.

| Key | Default | Notes |
|-----|---------|-------|
| `local.base_url` | `http://127.0.0.1:11434/v1` | OpenAI-compatible local server |
| `local.model` | `gemma4:e4b-mlx` | must match `ollama list` / your MLX tag |
| `local.reasoning_effort` | `none` | stops thinking models burning the output budget |
| `local.keep_alive` | `60m` | Ollama residency pin; `"-1"` forever; `""` off |
| `encoder.mode` | `hybrid` | `rules` \| `local` \| `hybrid` |
| `encoder.llm_scope` | `last_user` | `last_user` \| `all` \| `none` |
| `encoder.llm_timeout_s` | `15` | hard per-call timeout |
| `proxy.port` | `8787` | |
| `proxy.upstream_base_url` | paid API or `http://127.0.0.1:8317/v1` | Hermes OAuth proxy recommended when API keys are limited |

## A/B: local models

Corpus + Hermes fixtures, hybrid, 15s, warm. Results: [`docs/model-ab.md`](docs/model-ab.md).

```bash
MODELS="gemma4:e4b-mlx qwen3.5:4b-mlx" RUNS=3 scripts/ab_models.sh
```

## Safety / quality

- Paths, errors, IDs, and code evidence are preserved by construction (fence-safe rules) and by instruction (local-LLM system prompt).
- Truncation / timeout / empty LLM output → rules fallback; Hermes turn still completes.
- Loopback Host-header guard (DNS-rebinding protection).

## Develop

```bash
cargo test
cargo build --release   # LTO + strip
./scripts/setup-mac.sh
```

## License

MIT — see [LICENSE](LICENSE).

# Prompt Codec

Local **coder / decoder agent** that optimizes prompts on your Mac **before** they hit paid APIs, so you burn fewer tokens (and dollars).

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
          │
          ▼
┌───────────────────┐
│  DECODE (optional)│  ← expand terse replies on local model
└───────────────────┘
```

## What you get

| Piece | Role |
|-------|------|
| **Encoder (coder)** | Compress / densify prompts: rules + local model rewrite |
| **Decoder** | Optional expand of dense cloud replies (off by default) |
| **CLI** | One-shot `encode` / `decode` / `demo` / `health` |
| **Proxy** | OpenAI-compatible server on `:8787` — drop-in base URL |

## Install

```bash
cd ~/projects/prompt-codec
python3 -m pip install -e .
# or without install:
export PYTHONPATH=~/projects/prompt-codec
```

## Quick start (no local model needed)

```bash
cd ~/projects/prompt-codec
python3 -m prompt_codec.cli demo
python3 -m prompt_codec.cli encode --mode rules -f some_prompt.txt
```

## Local model (stronger savings)

Ollama is already on this Mac. Pull a small/medium instruct model, then set `config.yaml`:

```bash
ollama pull gemma3:4b   # or your preferred MLX/Ollama tag
# edit config.yaml → local.model
python3 -m prompt_codec.cli health
python3 -m prompt_codec.cli encode --mode hybrid "long prompt here..."
```

Also works with:

- **MLX-LM server**: `mlx_lm.server --model mlx-community/Qwen3.6-27B-4bit` → `http://127.0.0.1:8080/v1`
- **Exo**: `http://127.0.0.1:52415/v1` (cluster)

## Proxy (route any OpenAI client through the codec)

```bash
export X_API_KEY=...          # or whatever proxy.upstream_api_key_env is
python3 -m prompt_codec.cli proxy
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
| `rules` | No | Whitespace collapse, dedupe lines, strip fluff, trim long lists |
| `local` | Yes | Full rewrite by local model toward `target_ratio` |
| `hybrid` | Preferred | Rules first, then local refine (best default) |

Encoder **never answers the task** — it only rewrites the prompt.  
If local encode fails or expands tokens, it falls back to rules / original.

## Config

See `config.yaml` / `config.example.yaml`.

Key knobs:

- `encoder.target_ratio` — aim for e.g. `0.45` of original size  
- `encoder.min_chars_to_compress` — skip tiny messages  
- `encoder.protect_system_under_chars` — leave short system prompts alone  
- `stats.usd_per_mtok_input` — rough $ savings display only  

## Safety / quality

- Paths, errors, IDs, and code evidence are instructed to be preserved.
- Hybrid mode rejects local rewrites that don't actually save tokens.
- Start with `rules` on production traffic, A/B quality, then enable `hybrid`.
- For agent tool dumps (logs, HTML, JSON), rules alone often cut **50–90%**.

## Project layout

```
prompt-codec/
  config.yaml
  prompt_codec/
    codec.py       # encoder/decoder agent
    rules.py       # free deterministic compressor
    local_llm.py   # Ollama/MLX/Exo client
    proxy.py       # FastAPI OpenAI proxy
    cli.py         # Typer CLI
    tokens.py      # tiktoken stats
    config.py
```

## License

MIT (yours to use in ScaleBySEO / Hermes / DROID).

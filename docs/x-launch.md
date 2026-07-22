# X / launch kit (post after `v0.3.0` Release is live)

You post manually. Assets + copy below.

## Checklist

- [ ] GitHub Release `v0.3.0` published with `prompt-codec-v0.3.0-macos-arm64.tar.gz`
- [ ] README install block works on a clean Mac
- [ ] Hermes docs PR still open/linked: https://github.com/NousResearch/hermes-agent/pull/68835
- [ ] Provider plugin PR URL filled in below when opened
- [ ] One screenshot or terminal GIF: `curl -s localhost:8787/health | jq .totals` after a Telegram turn

## Thread (copy/paste)

**1/**
Hermes (and every coding agent) burns paid tokens on fluffy tool dumps and thank-you paragraphs.

I built **prompt-codec**: a local densify proxy that rewrites outbound prompts on your Mac *before* they hit xAI/OpenAI — complementary to Hermes’s own context compression.

**2/**
Flow:

Telegram / Hermes CLI → `localhost:8787` (rules + `gemma4:e4b-mlx`) → paid API

Fence-safe rules, optional MLX/Ollama refine, rewrite cache so prompt-cache prefixes stay byte-stable.

**3/**
Measured on Hermes-shaped traffic (warm, 15s budget):

- `gemma4:e4b-mlx` beats rules on every corpus file
- 14/14 planted-fact fidelity
- No fine-tune needed yet — residency + prompt discipline won

**4/**
Install (Apple Silicon):

```
cargo install --git https://github.com/jwaynelowry/prompt-codec
ollama pull gemma4:e4b-mlx
prompt-codec proxy
```

Repo: https://github.com/jwaynelowry/prompt-codec  
Release: https://github.com/jwaynelowry/prompt-codec/releases/tag/v0.3.0

**5/**
Hermes integration:

- Docs recipe PR: https://github.com/NousResearch/hermes-agent/pull/68835
- Provider plugin PR: https://github.com/NousResearch/hermes-agent/pull/69099

If you run Hermes + Ollama MLX, try densify on a long tool turn and watch `GET /health` totals climb. Feedback welcome.

## Hashtags (optional, spare)

`#HermesAgent` `#Ollama` `#MLX` `#LocalLLM` `#AppleSilicon`

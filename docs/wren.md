# Parked: Wren (purpose-built compressor)

Status: **parked** — not a default flip for prompt-codec.

## What it is

[baahaus/wren](https://github.com/baahaus/wren) is a tiny Apple Silicon / MLX
prompt compressor (LoRA on `Qwen2.5-1.5B-Instruct`) aimed at coding-agent
prompts and tool output. It claims 50–80% shrink while preserving paths, flags,
errors, and step ordering. It also ships as an MCP server.

## Why not now

- Not an Ollama OpenAI-compatible drop-in — would need a parallel MLX/Python
  backend beside the current Rust `LlmClient`.
- Very early project (~5 GitHub stars at research time); no Hermes-shaped A/B
  against our golden corpus yet.
- prompt-codec’s hybrid path (fence-safe rules + general SLM) already earns
  measurable savings for Hermes with a one-line model swap.

## When to reopen

Revisit if:

1. Wren exposes a stable OpenAI-compatible `/v1/chat/completions`, or
2. We want a dedicated MLX backend and can A/B it with `scripts/ab_models.sh`
   against `gemma4:e4b-mlx` on `tests/corpus/*` + the 14-fact fidelity probe.

Until then, keep the Ollama shortlist in [`model-ab.md`](model-ab.md).

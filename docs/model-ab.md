# Local model A/B (hybrid, Hermes traffic)

Placeholder — populated by `scripts/ab_models.sh` after the shortlist models
are pulled and the harness finishes.

## Protocol

- Mode: `hybrid`, `encoder.llm_timeout_s: 15`, `reasoning_effort: none`
- Warm model via Ollama `/api/generate` before timing
- Median of 3 timed runs per corpus file
- Corpus: `fluffy.txt`, `code_heavy.md`, `tool_dump.json`, `hermes_tool_turn.txt`
- Fidelity: 14 planted facts (paths, error string, TTL, IDs, etc.)

## Shortlist

| Model | Role |
|-------|------|
| `qwen3.5:4b-mlx` | Control / current default |
| `gemma4:e4b-mlx` | Instruction-following / fidelity candidate |
| `qwen3.5:9b-mlx` | In-family quality step-up |
| `lfm2.5:8b-a1b-q4_K_M` | Speed / IF dark horse (1.5B active MoE) |
| `gemma4:26b-mlx` | Optional only if shortlist truncates or misses facts |

## Decision rule

Pick the fastest model that: beats rules on every corpus file, never truncates
under budget, and scores **14/14** on the planted-fact probe (or 13/14 only
with an explicit accept). If none beat `qwen3.5:4b-mlx`, keep qwen.

## Results

_Pending harness run._

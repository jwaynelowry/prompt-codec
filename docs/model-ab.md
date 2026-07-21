# Local model A/B (hybrid, Hermes traffic)

Generated: 2026-07-21T22:10Z  
Protocol: warm model (unload others, `keep_alive=60m`), isolated `cache.persist: false`, median of 3 timed runs, `encoder.llm_timeout_s: 15`, `reasoning_effort: none`.  
Corpus: `fluffy.txt`, `code_heavy.md`, `tool_dump.json`, `hermes_tool_turn.txt` (+ 14-fact fidelity probe).

## Verdict

**Flip default to `gemma4:e4b-mlx`.**

| Model | Outcome |
|-------|---------|
| `gemma4:e4b-mlx` | **Winner.** Real `llm_encode` on every corpus file (no median rules fallback), **14/14** fidelity, no truncations. Slightly less aggressive than qwen on fluffy/hermes, but beats rules everywhere and is faster/more reliable on `code_heavy` + `tool_dump`. |
| `qwen3.5:4b-mlx` | Strong on fluffy/hermes when hot; **fails the decision rule** this run — `tool_dump.json` median stayed at rules-only (2/3 timeouts). Still a fine lighter fallback (~4 GB). |
| `qwen3.5:9b-mlx` | 14/14 fidelity, deep savings when it finishes, but median timeouts on `code_heavy` + `hermes_tool_turn` under 15s. Reject for default. |
| `lfm2.5:8b-a1b-q4_K_M` | **Reject.** Truncates (`finish_reason=length`) on every corpus file → silent rules-only. Fidelity 14/14 is rules-shaped, not a win. |
| `gemma4:26b-mlx` | **Not needed** — shortlist already has a non-truncating 14/14 winner. |

Decision rule: fastest model that beats rules on every corpus file, never truncates under budget, and scores 14/14. Only `gemma4:e4b-mlx` clears that bar.

## Results table

| file | model | before | after (median) | latency s (median) | notes |
|------|-------|--------|----------------|--------------------|-------|
| `fluffy.txt` | `qwen3.5:4b-mlx` | 255 | **83** | 2.54 | `llm_encode` |
| `code_heavy.md` | `qwen3.5:4b-mlx` | 981 | **222** | 8.81 | `llm_encode` (1/3 timeout) |
| `tool_dump.json` | `qwen3.5:4b-mlx` | 667 | 667 | 15.14 | 2/3 timeout → rules |
| `hermes_tool_turn.txt` | `qwen3.5:4b-mlx` | 662 | **156** | 5.65 | `llm_encode` |
| `fluffy.txt` | `gemma4:e4b-mlx` | 255 | **107** | 2.35 | `llm_encode` |
| `code_heavy.md` | `gemma4:e4b-mlx` | 981 | **293** | 4.61 | `llm_encode` (3/3) |
| `tool_dump.json` | `gemma4:e4b-mlx` | 667 | **331** | 5.35 | `llm_encode` (3/3) |
| `hermes_tool_turn.txt` | `gemma4:e4b-mlx` | 662 | **193** | 3.95 | `llm_encode` |
| `fluffy.txt` | `qwen3.5:9b-mlx` | 255 | **87** | 4.01 | `llm_encode` |
| `code_heavy.md` | `qwen3.5:9b-mlx` | 981 | 775 | 15.15 | 2/3 timeout → rules |
| `tool_dump.json` | `qwen3.5:9b-mlx` | 667 | **91** | 11.39 | `llm_encode` (1/3 timeout) |
| `hermes_tool_turn.txt` | `qwen3.5:9b-mlx` | 662 | 660 | 15.16 | 2/3 timeout → rules |
| `fluffy.txt` | `lfm2.5:8b-a1b-q4_K_M` | 255 | 175 | 4.51 | truncated → rules |
| `code_heavy.md` | `lfm2.5:8b-a1b-q4_K_M` | 981 | 775 | 7.08 | truncated → rules |
| `tool_dump.json` | `lfm2.5:8b-a1b-q4_K_M` | 667 | 667 | 6.49 | truncated → rules |
| `hermes_tool_turn.txt` | `lfm2.5:8b-a1b-q4_K_M` | 662 | 660 | 5.86 | truncated → rules |

### Fidelity probe (14 planted facts)

| model | score | notes |
|-------|-------|-------|
| `qwen3.5:4b-mlx` | **14/14** | 5.7s |
| `gemma4:e4b-mlx` | **14/14** | 3.6s |
| `qwen3.5:9b-mlx` | **14/14** | 7.5s |
| `lfm2.5:8b-a1b-q4_K_M` | 14/14 | 4.1s (rules-shaped; truncates on corpus) |

## Re-run

```bash
MODELS="qwen3.5:4b-mlx gemma4:e4b-mlx qwen3.5:9b-mlx lfm2.5:8b-a1b-q4_K_M" \
  RUNS=3 scripts/ab_models.sh
```

Harness notes: uses `/api/tags` (not `ollama show`), unloads other runners before warm, and uses an isolated non-persistent cache so SQLite hits do not fake results. Pre-warm Ollama if the server was just restarted — cold MLX loads can exceed 15s.

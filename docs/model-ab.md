# Local model A/B (hybrid, Hermes traffic)

Generated: 2026-07-21T18:42Z  
Protocol: warm model (unload others, `keep_alive=60m`), isolated `cache.persist: false`, median of 3 timed runs, `encoder.llm_timeout_s: 15`, `reasoning_effort: none`.  
Corpus: `fluffy.txt`, `code_heavy.md`, `tool_dump.json`, `hermes_tool_turn.txt` (+ 14-fact fidelity probe).

## Verdict

**Keep `qwen3.5:4b-mlx` as the default.**

| Model | Outcome |
|-------|---------|
| `qwen3.5:4b-mlx` | Wins. Real `llm_encode` savings on fluffy / tool_dump / hermes_tool_turn; **14/14** fidelity. `code_heavy.md` still hits the 15s wall (rules-only fallback) — accepted under the current timeout. |
| `lfm2.5:8b-a1b-q4_K_M` | **Reject.** Truncates (`finish_reason=length`) on every corpus file → silent rules-only. Fidelity probe 14/14 is not a win (rules already preserve planted facts). |
| `gemma4:e4b-mlx` / `qwen3.5:9b-mlx` | Not pulled in time for this run (8–9 GB each). Re-run with `scripts/ab_models.sh` after `ollama pull`. |
| `gemma4:26b-mlx` | **Not needed** — control did not miss facts; challenger failed via truncation, not fidelity. |

Decision rule from the research plan: fastest model that beats rules, never truncates under budget, and scores 14/14. Only qwen clears that bar among models tested.

## Results table

| file | model | before | after (median) | latency s (median) | notes |
|------|-------|--------|----------------|--------------------|-------|
| `fluffy.txt` | `qwen3.5:4b-mlx` | 255 | **92** | 8.47 | `llm_encode` |
| `code_heavy.md` | `qwen3.5:4b-mlx` | 981 | 775 | 15.49 | LLM timeout → rules |
| `tool_dump.json` | `qwen3.5:4b-mlx` | 667 | **62** | 6.44 | `llm_encode` |
| `hermes_tool_turn.txt` | `qwen3.5:4b-mlx` | 662 | **112** | 12.94 | `llm_encode` |
| `fluffy.txt` | `lfm2.5:8b-a1b-q4_K_M` | 255 | 175 | 7.39 | truncated → rules |
| `code_heavy.md` | `lfm2.5:8b-a1b-q4_K_M` | 981 | 775 | 14.24 | truncated / timeout → rules |
| `tool_dump.json` | `lfm2.5:8b-a1b-q4_K_M` | 667 | 667 | 12.96 | truncated → rules |
| `hermes_tool_turn.txt` | `lfm2.5:8b-a1b-q4_K_M` | 662 | 660 | 13.87 | truncated → rules |

### Fidelity probe (14 planted facts)

| model | score | notes |
|-------|-------|-------|
| `qwen3.5:4b-mlx` | **14/14** | 11.1s |
| `lfm2.5:8b-a1b-q4_K_M` | 14/14 | 8.0s (rules-shaped; truncates on corpus) |

## Re-run

```bash
ollama pull gemma4:e4b-mlx
ollama pull qwen3.5:9b-mlx
MODELS="qwen3.5:4b-mlx gemma4:e4b-mlx qwen3.5:9b-mlx lfm2.5:8b-a1b-q4_K_M" \
  RUNS=3 scripts/ab_models.sh
```

Harness notes: uses `/api/tags` (not `ollama show`), unloads other runners before warm, and uses an isolated non-persistent cache so SQLite hits do not fake results.

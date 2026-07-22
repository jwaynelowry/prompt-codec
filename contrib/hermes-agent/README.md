# Hermes Agent: prompt-codec provider plugin

Copy into a Hermes checkout (or `$HERMES_HOME/plugins/model-providers/`):

```bash
cp -R contrib/hermes-agent/plugins/model-providers/prompt-codec \
  /path/to/hermes-agent/plugins/model-providers/
```

Then:

1. Install and run [prompt-codec](https://github.com/jwaynelowry/prompt-codec) on `:8787`
2. In `~/.hermes/config.yaml`:

```yaml
providers:
  prompt_codec:
    api: http://127.0.0.1:8787/v1
    name: prompt_codec
    api_key: ${X_API_KEY}
    transport: chat_completions
model:
  provider: custom:prompt_codec   # or select via hermes model picker
  base_url: http://127.0.0.1:8787/v1
```

Complementary to Hermes `compression:` / ContextCompressor — do not disable mid-session compaction.

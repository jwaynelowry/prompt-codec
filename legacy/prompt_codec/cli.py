"""CLI: encode / decode / proxy / health / demo."""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Optional

import typer
from rich.console import Console
from rich.panel import Panel
from rich.table import Table

from .codec import PromptCodec
from .config import load_config
from .local_llm import LocalLLM

app = typer.Typer(
    name="prompt-codec",
    help="Local coder/decoder agent: compress prompts before paid LLM APIs.",
    add_completion=False,
)
console = Console()


def _codec(config: Optional[str]) -> PromptCodec:
    cfg = load_config(config)
    return PromptCodec(cfg, LocalLLM(cfg.local))


@app.command()
def encode(
    text: Optional[str] = typer.Argument(None, help="Prompt text (or use --file / stdin)"),
    file: Optional[Path] = typer.Option(None, "--file", "-f", help="Read prompt from file"),
    mode: Optional[str] = typer.Option(None, "--mode", "-m", help="rules | local | hybrid"),
    config: Optional[str] = typer.Option(None, "--config", "-c"),
    json_out: bool = typer.Option(False, "--json", help="Machine-readable output"),
):
    """Compress a prompt with the local coder (rules and/or local model)."""
    if file:
        raw = file.read_text()
    elif text:
        raw = text
    elif not sys.stdin.isatty():
        raw = sys.stdin.read()
    else:
        console.print("[red]Provide text, --file, or pipe stdin[/red]")
        raise typer.Exit(1)

    codec = _codec(config)
    if mode and mode not in ("rules", "local", "hybrid"):
        console.print("[red]mode must be rules|local|hybrid[/red]")
        raise typer.Exit(2)

    result = codec.encode_text(raw, mode=mode)  # type: ignore[arg-type]
    if json_out:
        print(json.dumps(result.as_dict(), indent=2))
        return

    s = result.stats
    console.print(Panel(result.text, title="Compressed prompt", border_style="green"))
    if s:
        t = Table(title="Token savings")
        t.add_column("Metric")
        t.add_column("Value", justify="right")
        t.add_row("Before", str(s.before_tokens))
        t.add_row("After", str(s.after_tokens))
        t.add_row("Saved", f"{s.saved_tokens} ({s.pct_saved:.1f}%)")
        t.add_row("Est. $ saved / call", f"${s.usd_saved:.6f}")
        t.add_row("Mode", result.mode_used)
        t.add_row("Notes", ", ".join(result.notes) or "—")
        console.print(t)


@app.command()
def decode(
    text: Optional[str] = typer.Argument(None),
    file: Optional[Path] = typer.Option(None, "--file", "-f"),
    config: Optional[str] = typer.Option(None, "--config", "-c"),
    force: bool = typer.Option(False, "--force", help="Decode even if decoder.enabled=false"),
):
    """Expand a dense cloud reply via the local decoder (optional)."""
    if file:
        raw = file.read_text()
    elif text:
        raw = text
    elif not sys.stdin.isatty():
        raw = sys.stdin.read()
    else:
        console.print("[red]Provide text, --file, or pipe stdin[/red]")
        raise typer.Exit(1)

    codec = _codec(config)
    if force:
        codec.cfg.decoder.enabled = True
        codec.cfg.decoder.mode = "local"
    result = codec.decode_text(raw)
    console.print(Panel(result.text, title="Decoded reply", border_style="cyan"))
    console.print(f"[dim]mode={result.mode_used} notes={result.notes}[/dim]")


@app.command()
def proxy(
    host: Optional[str] = typer.Option(None, "--host"),
    port: Optional[int] = typer.Option(None, "--port", "-p"),
    config: Optional[str] = typer.Option(None, "--config", "-c"),
):
    """Run OpenAI-compatible proxy: client → encode(local) → paid API."""
    from .proxy import run

    console.print("[bold]Starting Prompt Codec proxy…[/bold]")
    console.print("Point clients at http://HOST:PORT/v1 (OpenAI-compatible).")
    run(host=host, port=port, config_path=config)


@app.command()
def health(config: Optional[str] = typer.Option(None, "--config", "-c")):
    """Check local model endpoint + config."""
    cfg = load_config(config)
    llm = LocalLLM(cfg.local)
    h = llm.health()
    console.print_json(data={
        "config_encoder_mode": cfg.encoder.mode,
        "local": h,
        "upstream": cfg.proxy.upstream_base_url,
        "upstream_key_env": cfg.proxy.upstream_api_key_env,
    })
    raise typer.Exit(0 if h.get("ok") else 1)


@app.command()
def demo(config: Optional[str] = typer.Option(None, "--config", "-c")):
    """Run a rules-only demo on a verbose sample prompt (no local model needed)."""
    sample = """
Please, I would like you to help me with something very important. Thank you so much in advance!

I hope this helps set context. As an AI you are very capable.

I need you to refactor the authentication module in our codebase.

Important: Please remember to keep everything secure.

The file is at src/auth/session.py. There is also src/auth/session.py that handles sessions.
There is a bug where refresh tokens are not rotated. Error text:
  TokenRotationError: expected new jti, got reuse of abc123

Requirements:
- Rotate refresh tokens on every use
- Invalidate old token family on reuse detection
- Add unit tests
- Keep the public API stable
- Use existing logging helpers

Please also:
- Write clean code
- Add comments where needed
- Follow best practices
- Make it production ready
- Thank you!

Bullet dump of extra context that is somewhat redundant:
- auth uses JWT
- auth uses JWT
- refresh tokens live in Redis
- Redis key prefix: sess:
- TTL is 30 days
- TTL is 30 days
- We use FastAPI
- We use Python 3.11
- CI runs pytest
- Please be careful
- Note: this is important
"""
    codec = _codec(config)
    # Force rules so demo works without a local model
    result = codec.encode_text(sample, mode="rules")
    console.print(Panel(sample.strip(), title="BEFORE (verbose)", border_style="red"))
    console.print(Panel(result.text, title="AFTER (rules encode)", border_style="green"))
    if result.stats:
        console.print(
            f"[bold green]Saved {result.stats.saved_tokens} tokens "
            f"({result.stats.pct_saved:.1f}%) · est ${result.stats.usd_saved:.6f}/call[/bold green]"
        )
    console.print(
        "\n[dim]Tip: set encoder.mode=hybrid and pull a local Ollama/MLX model "
        "for stronger compression, then run: prompt-codec proxy[/dim]"
    )


if __name__ == "__main__":
    app()

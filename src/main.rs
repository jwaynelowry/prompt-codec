//! `prompt-codec` binary: thin parse-and-dispatch layer. All the actual
//! command logic lives in `prompt_codec::cli`.

use std::io::IsTerminal;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use prompt_codec::cli::{read_input, render_savings};
use prompt_codec::codec::Codec;
use prompt_codec::config::resolve_config;
use prompt_codec::llm::LlmClient;
use prompt_codec::proxy::create_app;
use prompt_codec::tokenizer::count_tokens;

#[derive(Parser)]
#[command(name = "prompt-codec", about = "Compress prompts before paid LLM APIs")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the OpenAI-compatible proxy
    Proxy {
        #[arg(long)]
        host: Option<String>,
        #[arg(short, long)]
        port: Option<u16>,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Compress a prompt (arg, --file, or stdin)
    Encode {
        text: Option<String>,
        #[arg(short, long)]
        file: Option<PathBuf>,
        #[arg(short, long)]
        mode: Option<String>,
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Rules-only demo on a bundled verbose sample
    Demo {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Check config + local model reachability (exit 1 if local model down)
    Health {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

/// The demo sample prompt, ported verbatim from
/// `legacy/prompt_codec/cli.py` (`sample = """..."""`, lines ~135-174).
const DEMO_SAMPLE: &str = r#"
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
"#;

fn print_config_warnings(warnings: &[String]) {
    for w in warnings {
        eprintln!("warning: {w}");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        // Logs go to stderr: stdout is reserved for command output so that
        // `encode --json | jq`-style pipes always receive clean JSON.
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Encode {
            text,
            file,
            mode,
            config,
            json,
        } => {
            if let Some(m) = mode.as_deref() {
                if !matches!(m, "rules" | "local" | "hybrid") {
                    eprintln!("mode must be rules|local|hybrid");
                    std::process::exit(2);
                }
            }

            let loaded = resolve_config(config)?;
            print_config_warnings(&loaded.warnings);

            let stdin_content =
                if file.is_none() && text.is_none() && !std::io::stdin().is_terminal() {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    Some(buf)
                } else {
                    None
                };
            let raw = read_input(text, file, stdin_content)?;

            let configured_mode = loaded.config.encoder.mode.clone();
            let codec = Codec::new(loaded.config);
            let (out, stats, notes) = codec.encode_text(&raw, mode.as_deref()).await;
            let mode_used = mode.as_deref().unwrap_or(configured_mode.as_str());

            if json {
                let payload = serde_json::json!({
                    "text": out,
                    "stats": stats,
                    "notes": notes,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("{out}");
                println!();
                println!("{}", render_savings(&stats, mode_used, &notes));
            }
        }

        Cmd::Demo { config } => {
            let loaded = resolve_config(config)?;
            print_config_warnings(&loaded.warnings);

            let codec = Codec::new(loaded.config);
            let sample = DEMO_SAMPLE;
            let (out, stats, _notes) = codec.encode_text(sample, Some("rules")).await;

            println!("BEFORE (verbose)");
            println!("----------------");
            println!("{}", sample.trim());
            println!();
            println!("AFTER (rules encode)");
            println!("---------------------");
            println!("{out}");
            println!();
            println!(
                "Saved {} tokens ({:.1}%) - est ${:.6}/call",
                stats.saved_tokens(),
                stats.pct_saved(),
                stats.usd_saved()
            );
            println!();
            println!(
                "Tip: set encoder.mode=hybrid and pull a local Ollama/MLX model for stronger compression, then run: prompt-codec proxy"
            );
        }

        Cmd::Health { config } => {
            let loaded = resolve_config(config)?;
            print_config_warnings(&loaded.warnings);

            let llm = LlmClient::new(&loaded.config.local, loaded.config.encoder.llm_timeout_s);
            let health = llm.health().await;
            let ok = health.ok;
            let payload = serde_json::json!({
                "config_source": loaded.source,
                "encoder_mode": loaded.config.encoder.mode,
                "local": health,
                "upstream": loaded.config.proxy.upstream_base_url,
                "upstream_key_env": loaded.config.proxy.upstream_api_key_env,
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
            if !ok {
                std::process::exit(1);
            }
        }

        Cmd::Proxy { host, port, config } => {
            let loaded = resolve_config(config)?;
            print_config_warnings(&loaded.warnings);

            let mut cfg = loaded.config;
            if let Some(h) = host {
                cfg.proxy.host = h;
            }
            if let Some(p) = port {
                cfg.proxy.port = p;
            }

            if !is_loopback_host(&cfg.proxy.host) {
                eprintln!(
                    "warning: proxy is binding to a non-loopback host ({}); \
                     this proxy has no auth of its own — anything reaching this port \
                     spends your upstream API key",
                    cfg.proxy.host
                );
            }

            // Load the BPE table now, not on the first request.
            count_tokens("warmup");

            let addr = format!("{}:{}", cfg.proxy.host, cfg.proxy.port);
            println!("Starting Prompt Codec proxy...");
            println!("Listening on http://{addr}/v1 (OpenAI-compatible)");
            println!("Config source: {}", loaded.source);
            println!("Upstream base: {}", cfg.proxy.upstream_base_url);

            let source = loaded.source.clone();
            let app = create_app(cfg, source);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
        }
    }

    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

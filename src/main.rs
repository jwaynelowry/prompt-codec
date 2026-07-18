//! `prompt-codec` binary: thin parse-and-dispatch layer. All the actual
//! command logic lives in `prompt_codec::cli`.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use prompt_codec::cli::{read_input, render_savings, Mode};
use prompt_codec::codec::Codec;
use prompt_codec::config::resolve_config;
use prompt_codec::llm::{keep_alive_loop, repin_interval, LlmClient};
use prompt_codec::proxy::{cfg_host_is_loopback, create_app_with_state, flush_totals};
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
        /// Bind host (overrides proxy.host from config)
        #[arg(long)]
        host: Option<String>,
        /// Bind port (overrides proxy.port from config)
        #[arg(short, long)]
        port: Option<u16>,
        /// Path to a config file (default: search chain, then built-in defaults)
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Compress a prompt (arg, --file, or stdin)
    Encode {
        /// Prompt text (or use --file / pipe stdin)
        text: Option<String>,
        /// Read the prompt from a file (wins over the text argument)
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Encoder mode override (default: encoder.mode from config)
        #[arg(short, long, value_enum)]
        mode: Option<Mode>,
        /// Path to a config file (default: search chain, then built-in defaults)
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// Emit machine-readable JSON ({"text", "stats", "notes"}) on stdout
        #[arg(long)]
        json: bool,
    },
    /// Rules-only demo on a bundled verbose sample
    Demo {
        /// Path to a config file (default: search chain, then built-in defaults)
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Check config + local model (exit 1 if unreachable or model not pulled)
    Health {
        /// Path to a config file (default: search chain, then built-in defaults)
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

/// The demo sample prompt, ported verbatim from
/// `legacy/prompt_codec/cli.py` (`sample = """..."""`, lines ~135-174).
/// Single-sourced from the golden corpus so the demo and the corpus tests
/// can never drift apart.
const DEMO_SAMPLE: &str = include_str!("../tests/corpus/fluffy.txt");

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
            let loaded = resolve_config(config)?;
            print_config_warnings(&loaded.warnings);

            let stdin_content =
                if file.is_none() && text.is_none() && !std::io::stdin().is_terminal() {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin()
                        .read_to_string(&mut buf)
                        .context("failed to read prompt from stdin")?;
                    // A 0-byte pipe is "no input", not an empty prompt — let
                    // read_input produce its proper error.
                    (!buf.is_empty()).then_some(buf)
                } else {
                    None
                };
            let raw = read_input(text, file, stdin_content)?;

            let codec = Codec::new(loaded.config);
            let result = codec.encode_text(&raw, mode.map(Mode::as_str)).await;

            if json {
                let payload = serde_json::json!({
                    "text": result.text,
                    "stats": result.stats,
                    "notes": result.notes,
                });
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("{}", result.text);
                println!();
                println!(
                    "{}",
                    render_savings(&result.stats, &result.mode_used, &result.notes)
                );
            }
        }

        Cmd::Demo { config } => {
            let loaded = resolve_config(config)?;
            print_config_warnings(&loaded.warnings);

            let codec = Codec::new(loaded.config);
            let sample = DEMO_SAMPLE;
            let result = codec.encode_text(sample, Some("rules")).await;

            println!("BEFORE (verbose)");
            println!("----------------");
            println!("{}", sample.trim());
            println!();
            println!("AFTER (rules encode)");
            println!("---------------------");
            println!("{}", result.text);
            println!();
            println!(
                "Saved {} tokens ({:.1}%) - est ${:.6}/call",
                result.stats.saved_tokens(),
                result.stats.pct_saved(),
                result.stats.usd_saved()
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
            let failed = prompt_codec::cli::health_failed(health.ok, health.model_present);
            let model_absent = health.ok && health.model_present == Some(false);
            let payload = serde_json::json!({
                "config_source": loaded.source,
                "encoder_mode": loaded.config.encoder.mode,
                "local": health,
                "upstream": loaded.config.proxy.upstream_base_url,
                "upstream_key_env": loaded.config.proxy.upstream_api_key_env,
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
            if model_absent {
                eprintln!(
                    "warning: local model '{}' is not pulled — hybrid/local modes will degrade to rules-only; run: ollama pull {}",
                    loaded.config.local.model, loaded.config.local.model
                );
            }
            if failed {
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

            if !cfg_host_is_loopback(&cfg.proxy.host) {
                eprintln!(
                    "warning: proxy is binding to a non-loopback host ({}); \
                     this proxy has no auth of its own — anything reaching this port \
                     spends your upstream API key",
                    cfg.proxy.host
                );
            }

            // Load the BPE table now, not on the first request.
            count_tokens("warmup");

            // Visibility, not a gate: in local/hybrid mode a missing local
            // model silently degrades every request to rules-only, so say so
            // loudly up front. The probe has its own 3s timeout; startup
            // proceeds regardless of the outcome.
            if matches!(cfg.encoder.mode.as_str(), "local" | "hybrid") {
                let llm = LlmClient::new(&cfg.local, cfg.encoder.llm_timeout_s);
                let h = llm.health().await;
                if !h.ok || h.model_present == Some(false) {
                    eprintln!(
                        "warning: local model '{}' unavailable — {} mode will degrade to \
                         rules-only; run: ollama pull {}",
                        cfg.local.model, cfg.encoder.mode, cfg.local.model
                    );
                }

                // Warm-model keep-alive pinner (v0.3): proxy only, local/hybrid
                // only — pinning a model `rules` mode never calls would waste
                // RAM. The probe above is done with `llm`, so it moves into the
                // spawned task; the loop runs concurrently with serving and
                // must never delay `axum::serve` below.
                if !cfg.local.keep_alive.is_empty() {
                    let keep_alive = cfg.local.keep_alive.clone();
                    let interval = repin_interval(&keep_alive);
                    tokio::spawn(keep_alive_loop(llm, keep_alive, interval));
                }
            }

            let addr = format!("{}:{}", cfg.proxy.host, cfg.proxy.port);
            println!("Starting Prompt Codec proxy...");
            println!("Listening on http://{addr}/v1 (OpenAI-compatible)");
            println!("Config source: {}", loaded.source);
            println!("Upstream base: {}", cfg.proxy.upstream_base_url);

            let source = loaded.source.clone();
            let (app, state) = create_app_with_state(cfg, source);
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .with_context(|| format!("failed to bind {addr}"))?;
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    tokio::signal::ctrl_c().await.ok();
                })
                .await?;
            // Graceful-shutdown (ctrl-c) flush: persist the final savings totals
            // so the next process picks them up. A hard kill may lose the last
            // few requests' counting — accepted per spec.
            flush_totals(&state);
        }
    }

    Ok(())
}

//! `prompt-codec doctor` — local model + upstream readiness checks for Hermes.

use serde::Serialize;

use crate::config::AppConfig;
use crate::llm::{LlmClient, LlmHealth};

/// Candidate OpenAI-compatible bases to probe when the configured one is down.
pub const LOCAL_BASE_CANDIDATES: &[&str] = &[
    "http://127.0.0.1:11434/v1", // Ollama
    "http://127.0.0.1:8080/v1",  // common MLX-LM
];

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub config_source: String,
    pub encoder_mode: String,
    pub local: LlmHealth,
    pub keep_alive: String,
    pub suggested_base_url: Option<String>,
    pub upstream: UpstreamCheck,
    pub hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpstreamCheck {
    pub base_url: String,
    pub ok: bool,
    pub status: Option<u16>,
    pub error: Option<String>,
}

/// Probe `/models` on a base URL with a short timeout. Returns whether the
/// endpoint looks like a working OpenAI-compatible server and whether `model`
/// appears in the listing (when the body shape is known).
pub async fn probe_base(base_url: &str, api_key: &str, model: &str) -> LlmHealth {
    let mut local = crate::config::LocalConfig::default();
    local.base_url = base_url.trim_end_matches('/').to_string();
    local.api_key = api_key.to_string();
    local.model = model.to_string();
    LlmClient::new(&local, 3.0).health().await
}

/// If `configured` is unhealthy, try [`LOCAL_BASE_CANDIDATES`] and return the
/// first healthy alternate (excluding the configured URL itself).
pub async fn suggest_local_base(
    configured: &str,
    api_key: &str,
    model: &str,
) -> Option<String> {
    let configured = configured.trim_end_matches('/');
    for cand in LOCAL_BASE_CANDIDATES {
        let c = cand.trim_end_matches('/');
        if c == configured {
            continue;
        }
        let h = probe_base(c, api_key, model).await;
        if h.ok {
            return Some(format!("{c}"));
        }
    }
    None
}

async fn check_upstream(base_url: &str) -> UpstreamCheck {
    let base = base_url.trim_end_matches('/');
    // Prefer /models; some proxies only expose /health.
    let urls = [
        format!("{base}/models"),
        format!("{}/health", base.trim_end_matches("/v1")),
    ];
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return UpstreamCheck {
                base_url: base_url.to_string(),
                ok: false,
                status: None,
                error: Some(e.to_string()),
            };
        }
    };
    let mut last_err = None;
    let mut last_status = None;
    for url in &urls {
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                last_status = Some(status);
                if status < 500 {
                    return UpstreamCheck {
                        base_url: base_url.to_string(),
                        ok: true,
                        status: Some(status),
                        error: None,
                    };
                }
                last_err = Some(format!("HTTP {status}"));
            }
            Err(e) => last_err = Some(e.to_string()),
        }
    }
    UpstreamCheck {
        base_url: base_url.to_string(),
        ok: false,
        status: last_status,
        error: last_err,
    }
}

/// Run the full doctor suite against a loaded config.
pub async fn run_doctor(cfg: &AppConfig, config_source: &str) -> DoctorReport {
    let llm = LlmClient::new(&cfg.local, cfg.encoder.llm_timeout_s);
    let local = llm.health().await;
    let suggested = if !local.ok {
        suggest_local_base(&cfg.local.base_url, &cfg.local.api_key, &cfg.local.model).await
    } else {
        None
    };
    let upstream = check_upstream(&cfg.proxy.upstream_base_url).await;

    let mut hints = Vec::new();
    if !local.ok {
        hints.push(format!(
            "local endpoint unreachable at {} — start Ollama (`ollama serve`) or MLX-LM",
            cfg.local.base_url
        ));
        if let Some(ref alt) = suggested {
            hints.push(format!(
                "found a healthy OpenAI-compatible server at {alt}; set local.base_url to that"
            ));
        }
    } else if local.model_present == Some(false) {
        hints.push(format!(
            "model '{}' not in listing — run: ollama pull {}",
            cfg.local.model, cfg.local.model
        ));
    }
    if cfg.local.keep_alive.is_empty()
        && matches!(cfg.encoder.mode.as_str(), "local" | "hybrid")
    {
        hints.push(
            "local.keep_alive is empty — cold MLX/Ollama loads may exceed llm_timeout_s; set keep_alive: \"60m\"".into(),
        );
    }
    if !upstream.ok {
        hints.push(format!(
            "upstream {} not reachable — for Hermes+xAI OAuth try http://127.0.0.1:8317/v1 (`hermes proxy start --provider xai`)",
            cfg.proxy.upstream_base_url
        ));
    }
    if matches!(cfg.encoder.mode.as_str(), "rules") {
        hints.push("encoder.mode is rules — no local model pin; densify is rules-only".into());
    }
    if hints.is_empty() {
        hints.push("ready for Hermes: point providers.prompt_codec.api at http://127.0.0.1:8787/v1".into());
    }

    let ok = local.ok
        && local.model_present != Some(false)
        && (cfg.encoder.mode == "rules" || local.ok);

    DoctorReport {
        ok,
        config_source: config_source.to_string(),
        encoder_mode: cfg.encoder.mode.clone(),
        local,
        keep_alive: cfg.local.keep_alive.clone(),
        suggested_base_url: suggested,
        upstream,
        hints,
    }
}

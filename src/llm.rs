//! Async client for a local OpenAI-compatible LLM (Ollama / MLX / llama.cpp).
//!
//! Two entry points matter to the rest of the crate:
//! - [`LlmClient::encode_text`] — the compression call, with a hard request
//!   timeout, a `max_tokens` budget sized to the job, and a truncation guard
//!   that rejects any `finish_reason == "length"` response outright (a
//!   truncated rewrite always "saves tokens" while silently dropping its tail).
//! - [`LlmClient::health`] — a non-failing `/models` probe used by `/health`.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use crate::config::LocalConfig;
use crate::tokenizer::count_tokens;

/// Ported from `legacy/prompt_codec/local_llm.py` (`ENCODE_SYSTEM`, the
/// 8-rule PROMPT COMPRESSOR system prompt), with rule 1 tightened on
/// 2026-07-18 after a fidelity probe showed qwen3.5:4b dropping a TTL:
/// durations/TTLs/timeouts, limits/thresholds, and quantities-with-units are
/// now named explicitly ("numbers" alone was not enough).
pub const ENCODE_SYSTEM: &str = r#"You are a PROMPT COMPRESSOR for paid LLM APIs.
Your job: rewrite the user's message so a strong cloud model still does the task correctly,
but with far fewer tokens.

Hard rules:
1. Preserve: goals, constraints, file paths, function/class names, error text, exact quotes, IDs, URLs, numbers, durations/TTLs/timeouts, limits and thresholds, quantities with their units, version numbers, acceptance criteria. Every concrete value in the original must appear in your output.
2. Remove: fluff, politeness, repetition, obvious commentary, restated instructions, markdown decoration that adds no info.
3. Prefer: short imperative bullets, dense technical English, tables only if denser than prose.
4. Do NOT answer the task. Only output the compressed prompt text.
5. Do NOT invent requirements. If unsure, keep the original phrase.
6. Keep code blocks if they are necessary evidence; otherwise summarize with path + signature + 1-line intent.
7. Target roughly the requested compression ratio, but never drop task-critical detail.
8. Output ONLY the compressed prompt — no preamble, no "here's the compressed version"."#;

/// Health snapshot of the configured local LLM. `#[derive(Serialize)]` because
/// `/health` (Task 11) embeds this struct directly in its JSON response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmHealth {
    pub ok: bool,
    pub status: Option<u16>,
    pub error: Option<String>,
    pub base_url: String,
    pub model: String,
    /// `Some(true/false)` when the `/models` body parsed as an OpenAI listing
    /// and we could check for the configured model; `None` when the listing was
    /// unavailable or in an unexpected shape (MLX servers vary) — purely
    /// informational, never gates `ok`.
    pub model_present: Option<bool>,
}

/// Async local-LLM client. Cheap to clone conceptually is not needed — the
/// codec holds a single instance for the process lifetime.
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    temperature: f64,
    max_tokens: u32,
    reasoning_effort: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Option<ChoiceMessage>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
}

/// Truncate to at most `max` chars (not bytes) for log hygiene — response
/// bodies are never emitted in full into error messages or logs.
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

impl LlmClient {
    pub fn new(cfg: &LocalConfig, timeout_s: f64) -> Self {
        // A hand-edited YAML can hold NaN/negative/overflowing values, and
        // `Duration::from_secs_f64` panics on those — guard so a bad
        // `encoder.llm_timeout_s` can't take down startup. Zero is treated as
        // invalid too (it would fail every request instantly).
        let timeout = Duration::try_from_secs_f64(timeout_s)
            .ok()
            .filter(|d| !d.is_zero())
            .unwrap_or_else(|| {
                tracing::warn!(
                    configured = timeout_s,
                    "invalid encoder.llm_timeout_s; clamping to 1s"
                );
                Duration::from_secs(1)
            });
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            temperature: cfg.temperature,
            max_tokens: cfg.max_tokens,
            reasoning_effort: cfg.reasoning_effort.clone(),
        }
    }

    /// The configured model name — the codec needs it to build cache keys.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Compress `text` toward `target_ratio` of its original token count.
    ///
    /// Errors (timeout, transport, non-2xx, unparseable body, truncated output,
    /// empty content) are returned as `Err`; the codec degrades to rules output
    /// on any error and never propagates it to the request.
    pub async fn encode_text(&self, text: &str, target_ratio: f64) -> anyhow::Result<String> {
        let pct = ((target_ratio * 100.0) as i64).clamp(5, 95);
        // The trailing reminder is load-bearing for small models: without it
        // qwen3.5:4b deterministically drops standalone value facts (e.g. a
        // "TTL is 30 days" bullet) — probed 2026-07-18, see README A/B notes.
        let user = format!(
            "Target length: about {pct}% of the original token count.\n\n--- ORIGINAL PROMPT ---\n{text}\n--- END ---\n\nCarry over EVERY concrete value from the original (paths, IDs, error text, durations, TTLs, limits, versions). A compressed prompt missing any of them is wrong."
        );
        // Size the output budget to the job so a legitimate compression fits,
        // capped by config. `max(256)` on the ceiling keeps clamp bounds valid
        // even under a pathologically small configured `max_tokens`.
        let budget = (count_tokens(text) as f64 * target_ratio * 1.5) as u32;
        let max_tokens = budget.clamp(256, self.max_tokens.max(256));

        let url = format!("{}/chat/completions", self.base_url);
        let mut payload = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": ENCODE_SYSTEM},
                {"role": "user", "content": user},
            ],
            "temperature": self.temperature,
            "max_tokens": max_tokens,
            "stream": false,
        });
        // Thinking models (Gemma 4, some Qwen) spend the whole output budget
        // on hidden reasoning unless told not to; Ollama's OpenAI endpoint
        // honors reasoning_effort. Omitted when configured empty, for servers
        // that reject unknown fields.
        if !self.reasoning_effort.is_empty() {
            payload["reasoning_effort"] = serde_json::Value::String(self.reasoning_effort.clone());
        }

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .context("local LLM request failed")?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .context("failed to read local LLM response body")?;
        if !status.is_success() {
            anyhow::bail!(
                "local LLM returned {}: {}",
                status,
                truncate_chars(&body, 200)
            );
        }

        let parsed: ChatResponse = serde_json::from_str(&body).with_context(|| {
            format!(
                "failed to parse local LLM response: {}",
                truncate_chars(&body, 200)
            )
        })?;
        let choice = parsed
            .choices
            .first()
            .context("local LLM response had no choices")?;
        // Truncation guard: a rewrite cut off at max_tokens always looks like a
        // token win, so reject it rather than forward a silently-clipped prompt.
        if choice.finish_reason.as_deref() == Some("length") {
            anyhow::bail!("local LLM output truncated (finish_reason=length)");
        }
        let content = choice
            .message
            .as_ref()
            .and_then(|m| m.content.as_deref())
            .unwrap_or_default();
        if content.trim().is_empty() {
            anyhow::bail!("local LLM returned empty content");
        }
        Ok(content.trim().to_string())
    }

    /// Probe `{base}/models`. Never errors — reachability failures become
    /// `ok: false` with the error text captured. Uses a 3 s per-call timeout
    /// override so `/health` stays responsive even when the process-wide
    /// request timeout is larger.
    pub async fn health(&self) -> LlmHealth {
        let url = format!("{}/models", self.base_url);
        let result = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .timeout(Duration::from_secs(3))
            .send()
            .await;
        match result {
            Ok(resp) => {
                let status = resp.status();
                // `Some(...)` only when the body is a genuine OpenAI models
                // listing — a JSON object with a `data` key. A `data` array is
                // checked for the configured model; `data: null` (Ollama with
                // zero pulled models) is a real, empty listing -> Some(false).
                // Anything else (non-JSON, `{"error": ...}`, exotic MLX
                // shapes) is `None`, never a false `Some(false)`.
                let model_present = match resp.json::<serde_json::Value>().await {
                    Ok(body) => match body.get("data") {
                        Some(serde_json::Value::Array(models)) => Some(models.iter().any(|m| {
                            m.get("id").and_then(serde_json::Value::as_str)
                                == Some(self.model.as_str())
                        })),
                        Some(serde_json::Value::Null) => Some(false),
                        _ => None,
                    },
                    Err(_) => None,
                };
                LlmHealth {
                    ok: status.as_u16() < 400,
                    status: Some(status.as_u16()),
                    error: None,
                    base_url: self.base_url.clone(),
                    model: self.model.clone(),
                    model_present,
                }
            }
            Err(e) => LlmHealth {
                ok: false,
                status: None,
                error: Some(e.to_string()),
                base_url: self.base_url.clone(),
                model: self.model.clone(),
                model_present: None,
            },
        }
    }
}

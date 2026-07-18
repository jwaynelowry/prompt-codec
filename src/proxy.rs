//! The axum reverse proxy: one streaming forward path that preserves the
//! upstream status and headers byte-for-byte.
//!
//! Design invariants (each fixes a verified defect in the legacy Python proxy):
//! - **Verbatim passthrough.** The upstream status, headers, and body bytes are
//!   forwarded unchanged. Streaming and non-streaming share ONE code path
//!   (`Body::from_stream`), so an upstream error can never be masked as a 200
//!   SSE stream, and SSE chunks are never buffered or rewritten.
//! - **No body mutation beyond `messages`/`prompt`.** Compression stats ride in
//!   `x-prompt-codec-*` response headers, never injected into the JSON body.
//! - **Fully async.** No blocking work on the request path.
//! - **DNS-rebinding guard.** When bound to loopback, requests whose `Host`
//!   header isn't itself loopback are rejected 403 — a malicious web page can't
//!   drive the localhost proxy and spend the user's upstream API key.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::codec::Codec;
use crate::config::{AppConfig, ProxyConfig};
use crate::stats::TokenStats;

/// Shared proxy state, built once in [`create_app`] and reused for the process
/// lifetime behind an `Arc`.
pub struct AppState {
    pub cfg: AppConfig,
    /// Config provenance string, reported verbatim by `/health` (Task 11).
    pub config_source: String,
    pub codec: Codec,
    /// Upstream HTTP client: a connect timeout but NO total timeout, so long
    /// streaming responses are never cut off mid-flight.
    pub upstream: reqwest::Client,
}

/// Build the router. `config_source` is carried into `AppState` for `/health`.
pub fn create_app(cfg: AppConfig, config_source: String) -> Router {
    let upstream = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build upstream reqwest client");
    let codec = Codec::new(cfg.clone());
    let state = Arc::new(AppState {
        cfg,
        config_source,
        codec,
        upstream,
    });

    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .route("/health", get(health))
        // axum 0.8 wildcard: catches every other /v1/* path, any method.
        .route("/v1/{*path}", any(catch_all))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            host_guard,
        ))
        .with_state(state)
}

// --- DNS-rebinding host guard ------------------------------------------------

/// Strip the port and any IPv6 brackets from a Host-header / authority value.
/// `127.0.0.1:8787` -> `127.0.0.1`, `[::1]:8787` -> `::1`, `localhost` -> `localhost`.
fn normalize_host(raw: &str) -> String {
    let h = raw.trim();
    if let Some(rest) = h.strip_prefix('[') {
        // Bracketed IPv6: `[::1]:port` or `[::1]`.
        return rest.split(']').next().unwrap_or(rest).to_string();
    }
    // A bare IPv6 literal (2+ colons) has no `host:port` split to make.
    if h.matches(':').count() >= 2 {
        return h.to_string();
    }
    match h.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            host.to_string()
        }
        _ => h.to_string(),
    }
}

/// Is a (normalized) host one of the loopback names we accept?
fn is_loopback_name(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

/// Is the *configured* bind host a loopback address (so the guard applies)?
fn cfg_host_is_loopback(host: &str) -> bool {
    is_loopback_name(&normalize_host(host))
}

/// Best-effort host of the incoming request: the `Host` header (HTTP/1.1) or
/// the URI authority (`:authority`, HTTP/2), normalized.
fn request_host(req: &Request) -> Option<String> {
    if let Some(h) = req.headers().get(header::HOST) {
        if let Ok(s) = h.to_str() {
            return Some(normalize_host(s));
        }
    }
    req.uri().host().map(normalize_host)
}

/// Reject non-loopback `Host` values when bound to loopback. A request that
/// carries no determinable host (non-browser clients) is allowed through — the
/// DNS-rebinding threat model is a browser `fetch`, which always sends `Host`.
async fn host_guard(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    if cfg_host_is_loopback(&state.cfg.proxy.host) {
        if let Some(h) = request_host(&request) {
            if !is_loopback_name(&h) {
                return error_response(
                    StatusCode::FORBIDDEN,
                    "forbidden",
                    "host header not permitted",
                );
            }
        }
    }
    next.run(request).await
}

// --- Error + header helpers --------------------------------------------------

/// An OpenAI-shaped error body: `{"error": {"message": ..., "type": ...}}`.
fn error_response(status: StatusCode, err_type: &str, message: &str) -> Response {
    let body = json!({ "error": { "message": message, "type": err_type } });
    (status, Json(body)).into_response()
}

/// Truncate to at most `max` chars (not bytes) — upstream error text is capped
/// before it reaches the client body.
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The three `x-prompt-codec-*` response headers derived from the compression
/// stats. `saved-pct` is formatted to one decimal place.
fn stat_headers(stats: &TokenStats) -> Vec<(HeaderName, HeaderValue)> {
    let mut out = Vec::with_capacity(3);
    if let Ok(v) = HeaderValue::from_str(&stats.before_tokens.to_string()) {
        out.push((HeaderName::from_static("x-prompt-codec-before"), v));
    }
    if let Ok(v) = HeaderValue::from_str(&stats.after_tokens.to_string()) {
        out.push((HeaderName::from_static("x-prompt-codec-after"), v));
    }
    if let Ok(v) = HeaderValue::from_str(&format!("{:.1}", stats.pct_saved())) {
        out.push((HeaderName::from_static("x-prompt-codec-saved-pct"), v));
    }
    out
}

// --- Auth resolution ---------------------------------------------------------

/// What Authorization to send upstream for this request.
enum AuthDecision {
    /// `require_client_auth` is set and the client sent none -> 401.
    Reject,
    /// Send this exact `Authorization` value.
    Header(HeaderValue),
    /// Send no `Authorization` header at all.
    None,
}

/// Resolve the upstream auth per config: reject when required-but-absent; else
/// forward the client's Authorization when `pass_client_auth`; else fall back
/// to `Bearer {env}` when the configured env var is non-empty.
fn resolve_auth(cfg: &ProxyConfig, client_auth: Option<&HeaderValue>) -> AuthDecision {
    if cfg.require_client_auth && client_auth.is_none() {
        return AuthDecision::Reject;
    }
    if cfg.pass_client_auth {
        if let Some(v) = client_auth {
            return AuthDecision::Header(v.clone());
        }
    }
    let env_val = std::env::var(&cfg.upstream_api_key_env).unwrap_or_default();
    if !env_val.is_empty() {
        if let Ok(hv) = HeaderValue::from_str(&format!("Bearer {env_val}")) {
            return AuthDecision::Header(hv);
        }
    }
    AuthDecision::None
}

// --- The one shared forward path ---------------------------------------------

/// Response headers that must not be copied verbatim: hop-by-hop framing that
/// axum re-derives for the streamed body.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    *name == header::TRANSFER_ENCODING
        || *name == header::CONNECTION
        || *name == header::CONTENT_LENGTH
}

/// Turn an upstream `reqwest::Response` into an axum `Response`, copying status
/// and (non-hop-by-hop) headers and streaming the body verbatim. `extra`
/// headers (the `x-prompt-codec-*` set) are appended for the compressing routes.
fn stream_response(resp: reqwest::Response, extra: Vec<(HeaderName, HeaderValue)>) -> Response {
    let status = resp.status();
    let src_headers = resp.headers().clone();
    // ONE body path for streaming and non-streaming: bytes pass through verbatim.
    let mut response = Response::new(Body::from_stream(resp.bytes_stream()));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    for (name, value) in src_headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }
    for (name, value) in extra {
        headers.insert(name, value);
    }
    response
}

/// Send `body` upstream to `url` with the given method, optional content-type,
/// and resolved auth; stream the reply back. The single forward path used by
/// the chat, completions, and catch-all routes.
async fn forward(
    state: &AppState,
    method: Method,
    url: String,
    content_type: Option<HeaderValue>,
    client_auth: Option<HeaderValue>,
    body: Bytes,
    extra: Vec<(HeaderName, HeaderValue)>,
) -> Response {
    let auth = match resolve_auth(&state.cfg.proxy, client_auth.as_ref()) {
        AuthDecision::Reject => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "client authentication required",
            )
        }
        AuthDecision::Header(v) => Some(v),
        AuthDecision::None => None,
    };

    let mut req = state.upstream.request(method, &url);
    if let Some(ct) = content_type {
        req = req.header(header::CONTENT_TYPE, ct);
    }
    if let Some(a) = auth {
        req = req.header(header::AUTHORIZATION, a);
    }
    req = req.body(body);

    match req.send().await {
        Ok(resp) => stream_response(resp, extra),
        Err(e) => error_response(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            &truncate_chars(&e.to_string(), 200),
        ),
    }
}

/// `{upstream_base}/{suffix}` with any trailing slash on the base trimmed.
fn upstream_url(state: &AppState, suffix: &str) -> String {
    format!(
        "{}/{}",
        state.cfg.proxy.upstream_base_url.trim_end_matches('/'),
        suffix.trim_start_matches('/')
    )
}

// --- Chat completions --------------------------------------------------------

/// `POST /v1/chat/completions`: compress `messages`, forward verbatim.
async fn chat_completions(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let client_auth = parts.headers.get(header::AUTHORIZATION).cloned();

    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "failed to read request body",
            )
        }
    };
    let mut payload: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "messages required",
            )
        }
    };
    let messages = match payload.get("messages") {
        Some(Value::Array(a)) if !a.is_empty() => a.clone(),
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "messages required",
            )
        }
    };

    let result = state.codec.encode_messages(messages).await;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("messages".to_string(), Value::Array(result.messages));
    }
    if state.cfg.proxy.log_stats {
        tracing::info!(
            before = result.stats.before_tokens,
            after = result.stats.after_tokens,
            pct_saved = result.stats.pct_saved(),
            notes = ?result.notes,
            "chat_completions encode",
        );
    }

    let new_body = match serde_json::to_vec(&payload) {
        Ok(b) => Bytes::from(b),
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "failed to serialize request body",
            )
        }
    };
    forward(
        &state,
        Method::POST,
        upstream_url(&state, "chat/completions"),
        Some(HeaderValue::from_static("application/json")),
        client_auth,
        new_body,
        stat_headers(&result.stats),
    )
    .await
}

// --- Completions -------------------------------------------------------------

/// `POST /v1/completions`: give a non-empty string `prompt` the user
/// compression treatment (LLM-eligible per config), then forward verbatim.
async fn completions(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let client_auth = parts.headers.get(header::AUTHORIZATION).cloned();

    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "failed to read request body",
            )
        }
    };
    let mut payload: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid request body",
            )
        }
    };

    let prompt = match payload.get("prompt") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    let mut extra = Vec::new();
    if let Some(prompt) = prompt {
        let (encoded, stats, notes) = state.codec.encode_text(&prompt, None).await;
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("prompt".to_string(), Value::String(encoded));
        }
        if state.cfg.proxy.log_stats {
            tracing::info!(
                before = stats.before_tokens,
                after = stats.after_tokens,
                pct_saved = stats.pct_saved(),
                notes = ?notes,
                "completions encode",
            );
        }
        extra = stat_headers(&stats);
    }

    let new_body = match serde_json::to_vec(&payload) {
        Ok(b) => Bytes::from(b),
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "failed to serialize request body",
            )
        }
    };
    forward(
        &state,
        Method::POST,
        upstream_url(&state, "completions"),
        Some(HeaderValue::from_static("application/json")),
        client_auth,
        new_body,
        extra,
    )
    .await
}

// --- Raw catch-all -----------------------------------------------------------

/// `any /v1/{*path}`: forward the original method, path, query, body bytes,
/// client content-type, and auth verbatim — NO JSON parsing, no body mutation,
/// no `x-prompt-codec-*` headers. Covers `/v1/embeddings`, `/v1/models`, etc.
async fn catch_all(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let method = parts.method;
    let content_type = parts.headers.get(header::CONTENT_TYPE).cloned();
    let client_auth = parts.headers.get(header::AUTHORIZATION).cloned();

    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "failed to read request body",
            )
        }
    };

    // Incoming paths are `/v1/...`; the upstream base is the v1 root, so strip
    // the `/v1` prefix and append the remainder (mirrors the chat/completions
    // routes appending their own suffix to the base).
    let path = parts.uri.path();
    let suffix = path.strip_prefix("/v1").unwrap_or(path);
    let mut url = upstream_url(&state, suffix);
    if let Some(q) = parts.uri.query() {
        url.push('?');
        url.push_str(q);
    }

    forward(
        &state,
        method,
        url,
        content_type,
        client_auth,
        bytes,
        Vec::new(),
    )
    .await
}

// --- Health ------------------------------------------------------------------

/// `GET /health`: encoder/upstream/config provenance, live cache size, and a
/// non-blocking local-LLM reachability probe. Never fails.
async fn health(State(state): State<Arc<AppState>>) -> Response {
    // Flush moka's pending write accounting so the count is accurate.
    state.codec.cache().sync();
    let cache_entries = state.codec.cache().entry_count();
    let local = serde_json::to_value(state.codec.llm().health().await).unwrap_or(Value::Null);
    let body = json!({
        "ok": true,
        "encoder_mode": state.cfg.encoder.mode,
        "upstream": state.cfg.proxy.upstream_base_url,
        "config_source": state.config_source,
        "cache_entries": cache_entries,
        "local": local,
    });
    (StatusCode::OK, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_host_strips_ports_and_brackets() {
        assert_eq!(normalize_host("127.0.0.1:8787"), "127.0.0.1");
        assert_eq!(normalize_host("localhost:8787"), "localhost");
        assert_eq!(normalize_host("[::1]:8787"), "::1");
        assert_eq!(normalize_host("[::1]"), "::1");
        assert_eq!(normalize_host("::1"), "::1");
        assert_eq!(normalize_host("evil.example.com"), "evil.example.com");
        assert_eq!(normalize_host("evil.example.com:443"), "evil.example.com");
    }

    #[test]
    fn loopback_classification() {
        assert!(is_loopback_name("127.0.0.1"));
        assert!(is_loopback_name("LOCALHOST"));
        assert!(is_loopback_name("::1"));
        assert!(!is_loopback_name("evil.example.com"));
        assert!(!is_loopback_name("10.0.0.5"));
        assert!(cfg_host_is_loopback("127.0.0.1"));
        assert!(!cfg_host_is_loopback("0.0.0.0"));
    }
}

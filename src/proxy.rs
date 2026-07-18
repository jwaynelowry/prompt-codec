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

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures_core::Stream;
use serde::Serialize;
use serde_json::{json, Value};

use crate::codec::Codec;
use crate::config::{AppConfig, ProxyConfig};
use crate::llm::LlmHealth;
use crate::stats::{round_to, TokenStats};
use crate::telemetry::{extract_cached_tokens, TailBuffer, Totals, TotalsSnapshot};

/// Shared proxy state, built once in [`create_app`] and reused for the process
/// lifetime behind an `Arc`.
pub struct AppState {
    pub cfg: AppConfig,
    /// Config provenance string, reported verbatim by `/health` (Task 11).
    pub config_source: String,
    pub codec: Codec,
    /// Upstream HTTP client: a connect timeout and a per-read (inter-chunk)
    /// timeout, but NO total timeout — a stalled half-open upstream is bounded
    /// without ever truncating a long *active* stream.
    pub upstream: reqwest::Client,
    /// `Bearer {key}` from the env var named by `proxy.upstream_api_key_env`,
    /// resolved once at startup. `None` when unset/empty.
    pub env_bearer: Option<HeaderValue>,
    /// Cumulative savings telemetry (v0.3 Feature 3): atomic counters loaded
    /// from the cache `meta` table at startup and flushed back on `/health`,
    /// every 32 counted requests, and at graceful shutdown.
    pub totals: Arc<Totals>,
}

/// Flush the current totals snapshot to the cache `meta` table. A no-op when
/// the cache has no disk tier (`cache.persist = false`), so it is always safe
/// to call. Serialization failure is swallowed — telemetry never fails a
/// request or shutdown.
pub fn flush_totals(state: &AppState) {
    flush_snapshot(state, &state.totals.snapshot());
}

/// Persist an already-taken snapshot — callers that also *report* the totals
/// (`/health`) take one snapshot and use it for both, rather than reading the
/// atomics twice.
fn flush_snapshot(state: &AppState, snap: &TotalsSnapshot) {
    if let Ok(json) = serde_json::to_string(snap) {
        state.codec.cache().meta_set("totals_json", &json);
    }
}

/// Build the router. `config_source` is carried into `AppState` for `/health`.
/// Thin wrapper over [`create_app_with_state`] for callers/tests that don't
/// need the shared state handle.
pub fn create_app(cfg: AppConfig, config_source: String) -> Router {
    create_app_with_state(cfg, config_source).0
}

/// Build the router AND return the shared [`AppState`] handle, so `main` can
/// flush telemetry after `axum::serve` returns on the graceful-shutdown path.
pub fn create_app_with_state(cfg: AppConfig, config_source: String) -> (Router, Arc<AppState>) {
    let upstream = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("failed to build upstream reqwest client");
    let env_bearer = std::env::var(&cfg.proxy.upstream_api_key_env)
        .ok()
        .filter(|v| !v.is_empty())
        .and_then(|v| HeaderValue::from_str(&format!("Bearer {v}")).ok());
    let codec = Codec::new(cfg.clone());
    // Lifetime totals: load a prior `totals_json` row (absent/corrupt → fresh,
    // warned inside `Totals::load`). Without a disk tier `meta_get` is `None`,
    // so totals are session-only with `since = process start`.
    let totals = Arc::new(Totals::load(codec.cache().meta_get("totals_json")));
    let state = Arc::new(AppState {
        cfg,
        config_source,
        codec,
        upstream,
        env_bearer,
        totals,
    });

    let router = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .route("/health", get(health))
        // axum 0.8 wildcard: catches every other /v1/* path, any method.
        .route("/v1/{*path}", any(catch_all))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            host_guard,
        ))
        .with_state(state.clone());
    (router, state)
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
/// `pub` because the CLI's `proxy` command reuses it for its non-loopback bind
/// warning — one classification, case-insensitive, port-tolerant.
pub fn cfg_host_is_loopback(host: &str) -> bool {
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

/// Request-body size cap shared by every read site.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Read a request body up to [`MAX_BODY_BYTES`], or produce the ready-to-return
/// 413 error response.
async fn read_body(body: Body) -> Result<Bytes, Response> {
    axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| {
            error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "request body too large (max 64MB)",
            )
        })
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
/// to the startup-resolved `Bearer {env}` value when present.
fn resolve_auth(
    cfg: &ProxyConfig,
    env_bearer: Option<&HeaderValue>,
    client_auth: Option<&HeaderValue>,
) -> AuthDecision {
    if cfg.require_client_auth && client_auth.is_none() {
        return AuthDecision::Reject;
    }
    if cfg.pass_client_auth {
        if let Some(v) = client_auth {
            return AuthDecision::Header(v.clone());
        }
    }
    if let Some(hv) = env_bearer {
        return AuthDecision::Header(hv.clone());
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

/// Pass-through response-body inspector for the compressing routes: it forwards
/// every chunk BYTE-FOR-BYTE while copying the trailing 16 KB into a
/// [`TailBuffer`]. On stream end — the final `poll_next` returning `None`, or a
/// drop if the client disconnects early — it scans the tail once for an upstream
/// cached-token count and folds it into the shared totals. No body buffering, no
/// added latency: the existing SSE byte-identity and causality tests are the
/// guard that this adds zero distortion.
struct InspectStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    tail: TailBuffer,
    totals: Arc<Totals>,
    finalized: bool,
}

impl InspectStream {
    fn new(
        inner: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
        totals: Arc<Totals>,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
            tail: TailBuffer::new(),
            totals,
            finalized: false,
        }
    }

    /// Run the tail scan exactly once (idempotent across a final poll + drop).
    fn finalize(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        if let Some(cached) = extract_cached_tokens(self.tail.as_bytes()) {
            self.totals.record_cache_info(cached);
        }
    }
}

impl Stream for InspectStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `InspectStream` is `Unpin` (every field is), so `get_mut` is sound.
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                this.tail.push(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                this.finalize();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for InspectStream {
    fn drop(&mut self) {
        // Early client disconnect: still scan whatever tail we captured.
        self.finalize();
    }
}

/// Turn an upstream `reqwest::Response` into an axum `Response`, copying status
/// and (non-hop-by-hop) headers and streaming the body verbatim. `extra`
/// headers (the `x-prompt-codec-*` set) are appended for the compressing routes.
/// When `inspect` is `Some`, the body stream is wrapped in an [`InspectStream`]
/// that captures upstream cached-token usage without altering the bytes.
fn stream_response(
    resp: reqwest::Response,
    extra: Vec<(HeaderName, HeaderValue)>,
    inspect: Option<Arc<Totals>>,
) -> Response {
    let status = resp.status();
    let src_headers = resp.headers().clone();
    // ONE body path for streaming and non-streaming: bytes pass through verbatim.
    // The inspector, when present, is a transparent tap — it never rewrites,
    // reorders, or withholds a chunk.
    let body = match inspect {
        Some(totals) => Body::from_stream(InspectStream::new(resp.bytes_stream(), totals)),
        None => Body::from_stream(resp.bytes_stream()),
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    for (name, value) in src_headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        // `append`, not `insert`: repeated upstream headers (Set-Cookie) must
        // all survive — collapsing to the last value would break verbatim
        // header passthrough.
        headers.append(name.clone(), value.clone());
    }
    for (name, value) in extra {
        headers.insert(name, value);
    }
    response
}

/// Send `body` upstream to `url` with the given method, optional content-type,
/// and resolved auth; stream the reply back. The single forward path used by
/// the chat, completions, and catch-all routes. `inspect` carries the totals
/// handle for the compressing routes (tail cached-token capture); the catch-all
/// passes `None`, so its traffic is never touched.
// The arg list is the request shape for the ONE shared forward path — keeping
// them positional (rather than a params struct) preserves the verbatim
// passthrough design and each call site reads as a plain forward.
#[allow(clippy::too_many_arguments)]
async fn forward(
    state: &AppState,
    method: Method,
    url: String,
    content_type: Option<HeaderValue>,
    client_auth: Option<HeaderValue>,
    body: Bytes,
    extra: Vec<(HeaderName, HeaderValue)>,
    inspect: Option<Arc<Totals>>,
) -> Response {
    let auth = match resolve_auth(
        &state.cfg.proxy,
        state.env_bearer.as_ref(),
        client_auth.as_ref(),
    ) {
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
        Ok(resp) => stream_response(resp, extra, inspect),
        Err(e) => {
            tracing::warn!(error = %e, %url, "upstream request failed");
            error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                &truncate_chars(&e.to_string(), 200),
            )
        }
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

    let bytes = match read_body(body).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let mut payload: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid JSON body",
            )
        }
    };
    // Move the array out (leaving an empty one behind) rather than deep-cloning
    // a potentially large message list; the encoded result is inserted back.
    let messages = match payload.get_mut("messages") {
        Some(Value::Array(a)) if !a.is_empty() => std::mem::take(a),
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
    // Telemetry: a valid messages array always produces stats here, so count it.
    record_and_maybe_flush(&state, &result.stats);

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
        Some(state.totals.clone()),
    )
    .await
}

/// Fold one compressed request's stats into the totals and, on every 32nd
/// counted request, flush them to the cache `meta` table. Shared by the two
/// compressing handlers (chat + completions).
fn record_and_maybe_flush(state: &AppState, stats: &TokenStats) {
    let n = state
        .totals
        .record_request(stats.before_tokens as u64, stats.after_tokens as u64);
    if n.is_multiple_of(32) {
        flush_totals(state);
    }
}

// --- Completions -------------------------------------------------------------

/// `POST /v1/completions`: give a non-empty string `prompt` the user
/// compression treatment (LLM-eligible per config), then forward verbatim.
async fn completions(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let client_auth = parts.headers.get(header::AUTHORIZATION).cloned();

    let bytes = match read_body(body).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let mut payload: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid JSON body",
            )
        }
    };

    let prompt = match payload.get("prompt") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    let mut extra = Vec::new();
    if let Some(prompt) = prompt {
        let result = state.codec.encode_text(&prompt, None).await;
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("prompt".to_string(), Value::String(result.text));
        }
        if state.cfg.proxy.log_stats {
            tracing::info!(
                before = result.stats.before_tokens,
                after = result.stats.after_tokens,
                pct_saved = result.stats.pct_saved(),
                notes = ?result.notes,
                "completions encode",
            );
        }
        // Telemetry: only a non-empty string prompt produces stats (a missing
        // or non-string `prompt` is a passthrough and must not be counted).
        record_and_maybe_flush(&state, &result.stats);
        extra = stat_headers(&result.stats);
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
    // Inspect the response only when we actually compressed (stats produced);
    // an uncompressed passthrough completion is not telemetry-counted.
    let inspect = (!extra.is_empty()).then(|| state.totals.clone());
    forward(
        &state,
        Method::POST,
        upstream_url(&state, "completions"),
        Some(HeaderValue::from_static("application/json")),
        client_auth,
        new_body,
        extra,
        inspect,
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

    let bytes = match read_body(body).await {
        Ok(b) => b,
        Err(resp) => return resp,
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
        None, // catch-all traffic is never inspected or counted
    )
    .await
}

// --- Health ------------------------------------------------------------------

/// The local-LLM section of `/health`: the probed [`LlmHealth`] flattened
/// together with the display-only configured keep-alive window (v0.3 warm-model
/// pinner). A typed wrapper — rather than a raw `serde_json::Value` mutation —
/// so the merged field names are compiler-checked.
#[derive(Serialize)]
struct LocalHealth {
    #[serde(flatten)]
    health: LlmHealth,
    /// Present only when pinning is configured on; `None` omits the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<String>,
}

/// `GET /health`: encoder/upstream/config provenance, live cache size, savings
/// totals, and a non-blocking local-LLM reachability probe. Never fails.
///
/// Side-effecting by design: every read also flushes the totals snapshot to
/// the cache `meta` table — flush trigger (a) of three — so reading health is
/// what guarantees the restart path carries the latest counters.
async fn health(State(state): State<Arc<AppState>>) -> Response {
    // ONE snapshot serves both the flush and the report below.
    let t = state.totals.snapshot();
    flush_snapshot(&state, &t);

    // Flush moka's pending write accounting so the count is accurate.
    state.codec.cache().sync();
    let cache_entries = state.codec.cache().entry_count();
    // Durable tier: entry count is None when persistence is off or the disk
    // cache is broken; path is None unless a durable tier is active.
    let cache_disk_entries = state.codec.cache().disk_entry_count();
    let cache_path = state
        .codec
        .cache()
        .disk_path()
        .map(|p| p.display().to_string());
    // Display-only: surface the configured keep-alive window without adding it
    // to `LlmHealth` itself — it's proxy config, not something the LLM probes.
    let local = LocalHealth {
        health: state.codec.llm().health().await,
        keep_alive: (!state.cfg.local.keep_alive.is_empty())
            .then(|| state.cfg.local.keep_alive.clone()),
    };
    let local = serde_json::to_value(local).unwrap_or(Value::Null);

    // Savings telemetry (v0.3 Feature 3): raw counters plus derived
    // saved_tokens / usd_saved_est. The USD key deliberately matches the
    // `usd_saved_est` name (and 6-decimal rounding) that stats.rs already
    // emits in `encode --json` — one name across every JSON surface.
    let usd = state.cfg.stats.usd_per_mtok_input;
    let totals = json!({
        "requests": t.requests,
        "before_tokens": t.before_tokens,
        "after_tokens": t.after_tokens,
        "saved_tokens": t.saved_tokens(),
        "usd_saved_est": round_to(t.usd_saved_est(usd), 6),
        "upstream_cached_tokens": t.upstream_cached_tokens,
        "responses_with_cache_info": t.responses_with_cache_info,
        "since": t.since,
    });

    let body = json!({
        "ok": true,
        "encoder_mode": state.cfg.encoder.mode,
        "upstream": state.cfg.proxy.upstream_base_url,
        "config_source": state.config_source,
        "cache_entries": cache_entries,
        "cache_disk_entries": cache_disk_entries,
        "cache_path": cache_path,
        "local": local,
        "totals": totals,
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

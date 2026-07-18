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

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
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

/// The `x-api-key` request-header name (Anthropic-style upstream auth). A
/// module constant so the client-passthrough read and the upstream-set share
/// one spelling.
const X_API_KEY: &str = "x-api-key";

/// Anthropic protocol version forwarded in `x_api_key` mode when the client
/// didn't send its own `anthropic-version` header.
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Cap on the in-memory recent-requests ring surfaced by the dashboard.
const RECENT_CAP: usize = 20;

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
    /// resolved once at startup. `None` when unset/empty. Used in `bearer`
    /// auth style.
    pub env_bearer: Option<HeaderValue>,
    /// The RAW upstream key from the same env var, resolved once at startup,
    /// sent verbatim as `x-api-key` in `x_api_key` auth style (v0.3 Feature 4).
    /// `None` when unset/empty.
    pub env_x_api_key: Option<HeaderValue>,
    /// Cumulative savings telemetry (v0.3 Feature 3): atomic counters loaded
    /// from the cache `meta` table at startup and flushed back on `/health`,
    /// every 32 counted requests, and at graceful shutdown.
    pub totals: Arc<Totals>,
    /// Session-only ring of the last [`RECENT_CAP`] compressed requests, shown
    /// by the draft dashboard (v0.3 Feature 4c). Not persisted.
    pub recent: Mutex<VecDeque<RecentEntry>>,
}

/// One row in the dashboard's recent-requests ring: the compression outcome of
/// a single request through a compressing route. Serialized directly into the
/// `/dashboard/data` `recent` array.
#[derive(Debug, Clone, Serialize)]
pub struct RecentEntry {
    pub ts_unix: u64,
    pub endpoint: String,
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub pct_saved: f64,
    /// A cached rewrite was reused (any `cache_hit` note).
    pub cache_hit: bool,
    /// The local model produced an accepted rewrite (any `llm_encode` note).
    pub llm_used: bool,
}

/// Per-instance meta key for the persisted totals row, namespaced by the
/// configured proxy port. Both shipped configs (xAI on 8787, the GLM demo on
/// 8788) share the default cache DB; without the namespace each proxy would
/// load and overwrite the SAME row — concurrent proxies silently erasing each
/// other's lifetime counts (last flush wins). Rewrite-row sharing across
/// proxies is untouched: that sharing is correct and desirable.
fn totals_meta_key(port: u16) -> String {
    format!("totals_json:{port}")
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
        state
            .codec
            .cache()
            .meta_set(&totals_meta_key(state.cfg.proxy.port), &json);
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
    // The upstream key is read once and pre-formatted into BOTH auth shapes so
    // the request path never re-reads the env or re-formats: `Bearer {key}` for
    // `bearer` style, the raw value for `x_api_key` style.
    let env_raw = std::env::var(&cfg.proxy.upstream_api_key_env)
        .ok()
        .filter(|v| !v.is_empty());
    let env_bearer = env_raw
        .as_deref()
        .and_then(|v| HeaderValue::from_str(&format!("Bearer {v}")).ok());
    let env_x_api_key = env_raw
        .as_deref()
        .and_then(|v| HeaderValue::from_str(v).ok());
    let codec = Codec::new(cfg.clone());
    // Lifetime totals: load this instance's port-namespaced `totals_json:{port}`
    // row (absent/corrupt → fresh, warned inside `Totals::load`). Without a
    // disk tier `meta_get` is `None`, so totals are session-only with
    // `since = process start`.
    let totals = Arc::new(Totals::load(
        codec.cache().meta_get(&totals_meta_key(cfg.proxy.port)),
    ));
    let state = Arc::new(AppState {
        cfg,
        config_source,
        codec,
        upstream,
        env_bearer,
        env_x_api_key,
        totals,
        recent: Mutex::new(VecDeque::with_capacity(RECENT_CAP)),
    });

    let router = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        // Anthropic passthrough (v0.3 Feature 4a): compress `messages`, forward
        // to `{base}/messages`. A specific route, so it wins over the wildcard.
        .route("/v1/messages", post(messages))
        .route("/health", get(health))
        // Draft dashboard (v0.3 Feature 4c): self-contained page + its poll feed.
        .route("/dashboard", get(dashboard))
        .route("/dashboard/data", get(dashboard_data))
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

/// The client-supplied request headers `forward` may need to relay upstream:
/// the two auth shapes plus the Anthropic protocol version. Extracted once per
/// handler from the incoming request via [`client_headers`].
#[derive(Default)]
struct ClientHeaders {
    authorization: Option<HeaderValue>,
    x_api_key: Option<HeaderValue>,
    anthropic_version: Option<HeaderValue>,
}

/// Pull the auth/version headers out of an incoming request's header map.
fn client_headers(headers: &HeaderMap) -> ClientHeaders {
    ClientHeaders {
        authorization: headers.get(header::AUTHORIZATION).cloned(),
        x_api_key: headers.get(X_API_KEY).cloned(),
        anthropic_version: headers.get("anthropic-version").cloned(),
    }
}

/// Is this config using the Anthropic-style `x-api-key` upstream auth?
fn is_x_api_key_style(cfg: &ProxyConfig) -> bool {
    cfg.upstream_auth_style == "x_api_key"
}

/// What auth header to send upstream for this request.
enum AuthDecision {
    /// `require_client_auth` is set and the client sent none -> 401.
    Reject,
    /// Send this exact header (`Authorization` or `x-api-key`).
    Header(HeaderName, HeaderValue),
    /// Send no auth header at all.
    None,
}

/// Resolve the upstream auth per config and auth style.
///
/// - `bearer` (default): reject when required-but-absent; else forward the
///   client's `Authorization` under `pass_client_auth`; else fall back to the
///   startup-resolved `Bearer {env}` value.
/// - `x_api_key` (Anthropic-style): `require_client_auth` is satisfied by
///   EITHER a client `x-api-key` or `Authorization`; passthrough forwards the
///   client's `x-api-key`; else fall back to the raw env key as `x-api-key`.
fn resolve_auth(
    cfg: &ProxyConfig,
    env_bearer: Option<&HeaderValue>,
    env_x_api_key: Option<&HeaderValue>,
    client: &ClientHeaders,
) -> AuthDecision {
    if is_x_api_key_style(cfg) {
        if cfg.require_client_auth && client.x_api_key.is_none() && client.authorization.is_none() {
            return AuthDecision::Reject;
        }
        if cfg.pass_client_auth {
            if let Some(v) = &client.x_api_key {
                return AuthDecision::Header(HeaderName::from_static(X_API_KEY), v.clone());
            }
        }
        if let Some(v) = env_x_api_key {
            return AuthDecision::Header(HeaderName::from_static(X_API_KEY), v.clone());
        }
        return AuthDecision::None;
    }

    if cfg.require_client_auth && client.authorization.is_none() {
        return AuthDecision::Reject;
    }
    if cfg.pass_client_auth {
        if let Some(v) = &client.authorization {
            return AuthDecision::Header(header::AUTHORIZATION, v.clone());
        }
    }
    if let Some(hv) = env_bearer {
        return AuthDecision::Header(header::AUTHORIZATION, hv.clone());
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
/// the chat, completions, messages, and catch-all routes. `inspect` carries the
/// totals handle for the compressing routes (tail cached-token capture); the
/// catch-all passes `None`, so its traffic is never touched.
// The arg list is the request shape for the ONE shared forward path — keeping
// them positional (rather than a params struct) preserves the verbatim
// passthrough design and each call site reads as a plain forward.
#[allow(clippy::too_many_arguments)]
async fn forward(
    state: &AppState,
    method: Method,
    url: String,
    content_type: Option<HeaderValue>,
    client: ClientHeaders,
    body: Bytes,
    extra: Vec<(HeaderName, HeaderValue)>,
    inspect: Option<Arc<Totals>>,
) -> Response {
    let auth = match resolve_auth(
        &state.cfg.proxy,
        state.env_bearer.as_ref(),
        state.env_x_api_key.as_ref(),
        &client,
    ) {
        AuthDecision::Reject => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "client authentication required",
            )
        }
        AuthDecision::Header(name, v) => Some((name, v)),
        AuthDecision::None => None,
    };

    let mut req = state.upstream.request(method, &url);
    if let Some(ct) = content_type {
        req = req.header(header::CONTENT_TYPE, ct);
    }
    if let Some((name, v)) = auth {
        req = req.header(name, v);
    }
    // Anthropic-style upstreams require an `anthropic-version` header; forward
    // the client's when present, else the documented default.
    if is_x_api_key_style(&state.cfg.proxy) {
        let ver = client
            .anthropic_version
            .unwrap_or_else(|| HeaderValue::from_static(DEFAULT_ANTHROPIC_VERSION));
        req = req.header("anthropic-version", ver);
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

// --- Shared compressing-handler steps -----------------------------------------

/// Parse a request body as JSON, or produce the ready-to-return 400 response.
// The Err IS the response (same contract as the async `read_body`); it's built
// once per malformed request, so its size is irrelevant — boxing would only
// add noise at every call site.
#[allow(clippy::result_large_err)]
fn parse_json_body(bytes: &Bytes) -> Result<Value, Response> {
    serde_json::from_slice(bytes).map_err(|_| {
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid JSON body",
        )
    })
}

/// Serialize the (mutated) payload back to bytes, or produce the ready-to-return
/// 400 response.
// See `parse_json_body` for the allow rationale.
#[allow(clippy::result_large_err)]
fn payload_to_bytes(payload: &Value) -> Result<Bytes, Response> {
    serde_json::to_vec(payload).map(Bytes::from).map_err(|_| {
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "failed to serialize request body",
        )
    })
}

/// Per-request stats log line, gated on `proxy.log_stats`. One shape for all
/// three compressing routes; `route` disambiguates.
fn log_encode(state: &AppState, route: &str, stats: &TokenStats, notes: &[String]) {
    if state.cfg.proxy.log_stats {
        tracing::info!(
            route,
            before = stats.before_tokens,
            after = stats.after_tokens,
            pct_saved = stats.pct_saved(),
            notes = ?notes,
            "encode",
        );
    }
}

/// The compress step shared by the two messages-list routes (OpenAI chat +
/// Anthropic messages — the payload shapes align for the codec): take the
/// non-empty `messages` array out of `payload` (else the 400), encode it,
/// insert the result back, then log / count telemetry / push the recent-ring
/// entry. Returns the `x-prompt-codec-*` headers for the forward call. Every
/// other payload key (`model`, `system`, ...) is left exactly as received.
async fn compress_messages_in_payload(
    state: &AppState,
    payload: &mut Value,
    endpoint: &'static str,
) -> Result<Vec<(HeaderName, HeaderValue)>, Response> {
    // Move the array out (leaving an empty one behind) rather than deep-cloning
    // a potentially large message list; the encoded result is inserted back.
    let messages = match payload.get_mut("messages") {
        Some(Value::Array(a)) if !a.is_empty() => std::mem::take(a),
        _ => {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "messages required",
            ))
        }
    };

    let result = state.codec.encode_messages(messages).await;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("messages".to_string(), Value::Array(result.messages));
    }
    log_encode(state, endpoint, &result.stats, &result.notes);
    // Telemetry: a valid messages array always produces stats here, so count it.
    record_and_maybe_flush(state, &result.stats);
    push_recent(state, endpoint, &result.stats, &result.notes);
    Ok(stat_headers(&result.stats))
}

// --- Chat completions --------------------------------------------------------

/// `POST /v1/chat/completions`: compress `messages`, forward verbatim.
async fn chat_completions(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let client = client_headers(&parts.headers);

    let bytes = match read_body(body).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let mut payload = match parse_json_body(&bytes) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let extra =
        match compress_messages_in_payload(&state, &mut payload, "/v1/chat/completions").await {
            Ok(headers) => headers,
            Err(resp) => return resp,
        };
    let new_body = match payload_to_bytes(&payload) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    forward(
        &state,
        Method::POST,
        upstream_url(&state, "chat/completions"),
        Some(HeaderValue::from_static("application/json")),
        client,
        new_body,
        extra,
        Some(state.totals.clone()),
    )
    .await
}

/// Fold one compressed request's stats into the totals and, on every 32nd
/// counted request, flush them to the cache `meta` table. Shared by all three
/// compressing handlers (chat + completions + messages).
fn record_and_maybe_flush(state: &AppState, stats: &TokenStats) {
    let n = state
        .totals
        .record_request(stats.before_tokens as u64, stats.after_tokens as u64);
    if n.is_multiple_of(32) {
        flush_totals(state);
    }
}

/// Push one compressed request onto the dashboard's recent-requests ring
/// (session-only, cap [`RECENT_CAP`]). `cache_hit`/`llm_used` are derived from
/// the codec's per-message note trail (`cache_hit*` / `llm_encode*` substrings).
/// Shared by all three compressing handlers.
fn push_recent(state: &AppState, endpoint: &str, stats: &TokenStats, notes: &[String]) {
    let entry = RecentEntry {
        ts_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        endpoint: endpoint.to_string(),
        before_tokens: stats.before_tokens as u64,
        after_tokens: stats.after_tokens as u64,
        pct_saved: stats.pct_saved(),
        cache_hit: notes.iter().any(|n| n.contains("cache_hit")),
        llm_used: notes.iter().any(|n| n.contains("llm_encode")),
    };
    let mut ring = state.recent.lock().unwrap_or_else(|e| e.into_inner());
    if ring.len() >= RECENT_CAP {
        ring.pop_front();
    }
    ring.push_back(entry);
}

// --- Completions -------------------------------------------------------------

/// `POST /v1/completions`: give a non-empty string `prompt` the user
/// compression treatment (LLM-eligible per config), then forward verbatim.
async fn completions(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let client = client_headers(&parts.headers);

    let bytes = match read_body(body).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let mut payload = match parse_json_body(&bytes) {
        Ok(v) => v,
        Err(resp) => return resp,
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
        log_encode(&state, "/v1/completions", &result.stats, &result.notes);
        // Telemetry: only a non-empty string prompt produces stats (a missing
        // or non-string `prompt` is a passthrough and must not be counted).
        record_and_maybe_flush(&state, &result.stats);
        push_recent(&state, "/v1/completions", &result.stats, &result.notes);
        extra = stat_headers(&result.stats);
    }

    let new_body = match payload_to_bytes(&payload) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    // Inspect the response only when we actually compressed (stats produced);
    // an uncompressed passthrough completion is not telemetry-counted.
    let inspect = (!extra.is_empty()).then(|| state.totals.clone());
    forward(
        &state,
        Method::POST,
        upstream_url(&state, "completions"),
        Some(HeaderValue::from_static("application/json")),
        client,
        new_body,
        extra,
        inspect,
    )
    .await
}

// --- Anthropic passthrough ---------------------------------------------------

/// `POST /v1/messages`: the Anthropic messages shape (v0.3 Feature 4a). Compress
/// `messages` with the SAME codec as chat — the shapes align: `user` string
/// content and `{"type":"text"}` blocks get the user treatment, `tool_result`
/// and other block types pass through untouched, and the top-level `system`
/// field is left alone. Forward verbatim to `{base}/messages`.
async fn messages(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let client = client_headers(&parts.headers);

    let bytes = match read_body(body).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let mut payload = match parse_json_body(&bytes) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let extra = match compress_messages_in_payload(&state, &mut payload, "/v1/messages").await {
        Ok(headers) => headers,
        Err(resp) => return resp,
    };
    let new_body = match payload_to_bytes(&payload) {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    forward(
        &state,
        Method::POST,
        upstream_url(&state, "messages"),
        Some(HeaderValue::from_static("application/json")),
        client,
        new_body,
        extra,
        Some(state.totals.clone()),
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
    let client = client_headers(&parts.headers);

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
        client,
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

    let totals = build_totals_json(&state, &t);

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

/// The savings-telemetry `totals` object shared by `/health` and
/// `/dashboard/data` (v0.3 Feature 3). Raw counters plus derived
/// `saved_tokens` / `usd_saved_est`. The USD key deliberately matches the
/// `usd_saved_est` name (and 6-decimal rounding) that stats.rs emits in
/// `encode --json` — one name across every JSON surface (pinned by test).
fn build_totals_json(state: &AppState, t: &TotalsSnapshot) -> Value {
    let usd = state.cfg.stats.usd_per_mtok_input;
    json!({
        "requests": t.requests,
        "before_tokens": t.before_tokens,
        "after_tokens": t.after_tokens,
        "saved_tokens": t.saved_tokens(),
        "usd_saved_est": round_to(t.usd_saved_est(usd), 6),
        "upstream_cached_tokens": t.upstream_cached_tokens,
        "responses_with_cache_info": t.responses_with_cache_info,
        "since": t.since,
    })
}

// --- Draft dashboard (v0.3 Feature 4c) ---------------------------------------

/// `GET /dashboard`: the self-contained draft dashboard page, compiled into the
/// binary (no build step, no external assets — works offline and under the host
/// guard). It fetches `/health` once for the header and polls `/dashboard/data`
/// every 2 s for the live totals and recent-requests table.
async fn dashboard() -> Response {
    Html(include_str!("dashboard.html")).into_response()
}

/// `GET /dashboard/data`: the dashboard's poll feed — the savings `totals`
/// object, cache tier sizes, and the recent-requests ring (newest first).
/// Deliberately does NOT run the local-LLM health probe (that lives on
/// `/health`) so a 2 s poll never blocks on the model.
async fn dashboard_data(State(state): State<Arc<AppState>>) -> Response {
    let t = state.totals.snapshot();
    let totals = build_totals_json(&state, &t);

    state.codec.cache().sync();
    let cache_entries = state.codec.cache().entry_count();
    let cache_disk_entries = state.codec.cache().disk_entry_count();

    // Newest first: the ring pushes newest at the back, so reverse on read.
    let recent: Vec<RecentEntry> = {
        let ring = state.recent.lock().unwrap_or_else(|e| e.into_inner());
        ring.iter().rev().cloned().collect()
    };

    let body = json!({
        "totals": totals,
        "cache_entries": cache_entries,
        "cache_disk_entries": cache_disk_entries,
        "recent": recent,
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

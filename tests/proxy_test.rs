// Config is built via `Default` + field assignment throughout these tests;
// the resulting style lint is noise on test scaffolding, not the assertions.
#![allow(clippy::field_reassign_with_default)]

use prompt_codec::config::AppConfig;
use reqwest::Client;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Spawn the proxy on an ephemeral loopback port and return its base URL.
/// TCP_NODELAY on accepted connections (the axum-documented `tap_io` pattern)
/// keeps Nagle/delayed-ACK stalls out of the streaming-latency test.
async fn spawn_proxy(cfg: AppConfig) -> String {
    use axum::serve::ListenerExt;
    let app = prompt_codec::proxy::create_app(cfg, "test-config".into());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = listener.tap_io(|tcp_stream| {
        tcp_stream.set_nodelay(true).ok();
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A fluffy user message: rules compression strips the trailing "Thank you so
/// much in advance!" while keeping the real content (the file path).
const FLUFFY: &str = "Please help me refactor the authentication logic in src/auth.rs so that it validates tokens correctly and handles expiry. The current implementation has a bug where expired tokens are still accepted, which is a security problem we must fix. Thank you so much in advance!";

fn rules_cfg(upstream_base: &str) -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.encoder.mode = "rules".into(); // deterministic, no local LLM needed
    cfg.proxy.upstream_base_url = upstream_base.to_string();
    // Hermetic: never touch the user's real cache dir during tests.
    cfg.cache.persist = false;
    cfg
}

#[tokio::test]
async fn compresses_messages_and_forwards_status_and_headers() {
    const RESP_BODY: &str = r#"{"id":"chatcmpl-xyz","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"done"}}]}"#;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("x-upstream-tag", "hello")
                .set_body_string(RESP_BODY),
        )
        .mount(&server)
        .await;

    let proxy = spawn_proxy(rules_cfg(&format!("{}/v1", server.uri()))).await;
    let client = Client::new();
    let resp = client
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "grok-2",
            "messages": [{"role": "user", "content": FLUFFY}],
            "temperature": 0.7,
        }))
        .send()
        .await
        .unwrap();

    // Response status and headers pass through verbatim, plus our stat headers.
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.headers().contains_key("x-prompt-codec-before"));
    assert!(resp.headers().contains_key("x-prompt-codec-after"));
    assert!(resp.headers().contains_key("x-prompt-codec-saved-pct"));
    assert_eq!(resp.headers().get("x-upstream-tag").unwrap(), "hello");
    let body = resp.text().await.unwrap();
    assert_eq!(body, RESP_BODY); // byte-identical upstream body

    // Upstream saw the compressed messages, and NO injected stats.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let content = sent["messages"][0]["content"].as_str().unwrap();
    assert!(
        !content.contains("Thank you"),
        "rules should have stripped the thank-you fluff; got: {content}"
    );
    assert!(content.contains("src/auth.rs"), "real content preserved");
    assert!(
        sent.get("metadata").is_none(),
        "stats must not be injected into the upstream body"
    );
    assert_eq!(sent["model"], "grok-2"); // untouched fields survive
}

#[tokio::test]
async fn upstream_401_and_429_reach_client_unchanged() {
    // Case A: 401 with an OpenAI-style error body passes through verbatim.
    const ERR_401: &str = r#"{"error":{"message":"Invalid API key","type":"authentication_error","code":"invalid_api_key"}}"#;
    let s401 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("content-type", "application/json")
                .set_body_string(ERR_401),
        )
        .mount(&s401)
        .await;
    let proxy401 = spawn_proxy(rules_cfg(&format!("{}/v1", s401.uri()))).await;

    let client = Client::new();
    let r = client
        .post(format!("{proxy401}/v1/chat/completions"))
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "hi there friend"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    assert_eq!(r.text().await.unwrap(), ERR_401);

    // Case B: 429 with a retry-after header and distinct body.
    const ERR_429: &str = r#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#;
    let s429 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7")
                .insert_header("content-type", "application/json")
                .set_body_string(ERR_429),
        )
        .mount(&s429)
        .await;
    let proxy429 = spawn_proxy(rules_cfg(&format!("{}/v1", s429.uri()))).await;
    let r = client
        .post(format!("{proxy429}/v1/chat/completions"))
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "hi there friend"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 429);
    assert_eq!(r.headers().get("retry-after").unwrap(), "7");
    assert_eq!(r.text().await.unwrap(), ERR_429);
}

#[tokio::test]
async fn client_auth_passthrough_and_env_key_fallback() {
    let client = Client::new();

    // Case 1: client Authorization is forwarded verbatim (pass_client_auth).
    let s1 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer client-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&s1)
        .await;
    let p1 = spawn_proxy(rules_cfg(&format!("{}/v1", s1.uri()))).await;
    let r = client
        .post(format!("{p1}/v1/chat/completions"))
        .header("authorization", "Bearer client-key")
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "hello world here"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "client key should be forwarded");

    // Case 2: no client auth -> Bearer from the configured env var.
    // Unique env var name (process-global; tests run in parallel).
    let env_name = "PROMPT_CODEC_TEST_AUTH_ENVKEY_FALLBACK";
    std::env::set_var(env_name, "env-key");
    let s2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer env-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&s2)
        .await;
    let mut cfg = rules_cfg(&format!("{}/v1", s2.uri()));
    cfg.proxy.upstream_api_key_env = env_name.to_string();
    let p2 = spawn_proxy(cfg).await;
    let r = client
        .post(format!("{p2}/v1/chat/completions"))
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "hello world here"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "env key should be used");
}

#[tokio::test]
async fn require_client_auth_gates_unauthenticated_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let mut cfg = rules_cfg(&format!("{}/v1", server.uri()));
    cfg.proxy.require_client_auth = true;
    let proxy = spawn_proxy(cfg).await;
    let client = Client::new();

    // (a) No Authorization -> 401 in the OpenAI shape, upstream never touched.
    let r = client
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "hello world here"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["error"]["type"], "authentication_error");
    assert!(body["error"]["message"].is_string());
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "unauthenticated request must never reach upstream"
    );

    // (b) With Authorization -> forwarded normally.
    let r = client
        .post(format!("{proxy}/v1/chat/completions"))
        .header("authorization", "Bearer gate-key")
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "hello world here"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer gate-key"
    );
}

#[tokio::test]
async fn missing_messages_is_400_openai_shape() {
    let proxy = spawn_proxy(rules_cfg("http://127.0.0.1:9/v1")).await;
    let client = Client::new();
    let r = client
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["error"]["message"], "messages required");
    assert_eq!(body["error"]["type"], "invalid_request_error");

    // An unparseable body is distinguished from a missing `messages` key.
    let r = client
        .post(format!("{proxy}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["error"]["message"], "invalid JSON body");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn upstream_down_is_502_openai_shape() {
    // Port 1 is unroutable -> connect error -> 502 upstream_error.
    let proxy = spawn_proxy(rules_cfg("http://127.0.0.1:1/v1")).await;
    let client = Client::new();
    let r = client
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "hello world here"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 502);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["error"]["type"], "upstream_error");
    assert!(body["error"]["message"].is_string());
}

#[tokio::test]
async fn non_loopback_host_header_is_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let proxy = spawn_proxy(rules_cfg(&format!("{}/v1", server.uri()))).await;
    let client = Client::new();

    // DNS-rebinding guard: a non-loopback Host is rejected before forwarding.
    let r = client
        .post(format!("{proxy}/v1/chat/completions"))
        .header("host", "evil.example.com")
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "hello world here"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "rejected request must never reach upstream"
    );

    // A loopback Host (reqwest's default 127.0.0.1:port) passes through.
    let r = client
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&serde_json::json!({"messages": [{"role": "user", "content": "hello world here"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
}

// --- Task 11: completions, catch-all, SSE, health ---------------------------

#[tokio::test]
async fn completions_prompt_gets_user_treatment() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let proxy = spawn_proxy(rules_cfg(&format!("{}/v1", server.uri()))).await;
    let client = Client::new();
    let r = client
        .post(format!("{proxy}/v1/completions"))
        .json(&serde_json::json!({"prompt": FLUFFY, "model": "m"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let prompt = sent["prompt"].as_str().unwrap();
    assert!(
        !prompt.contains("Thank you"),
        "prompt should get the user compression treatment; got: {prompt}"
    );
    assert!(prompt.contains("src/auth.rs"), "real content preserved");
    assert_eq!(sent["model"], "m", "other fields untouched");
}

#[tokio::test]
async fn catch_all_forwards_raw_body_query_and_method() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header("x-up", "1")
                .set_body_string("resp-body"),
        )
        .mount(&server)
        .await;
    let proxy = spawn_proxy(rules_cfg(&format!("{}/v1", server.uri()))).await;
    let client = Client::new();

    let raw: &[u8] = b"rawbytes\x00\x01";
    let r = client
        .post(format!("{proxy}/v1/embeddings?foo=bar"))
        .header("content-type", "application/x-raw-test")
        .body(raw.to_vec())
        .send()
        .await
        .unwrap();

    // Response passes through unchanged.
    assert_eq!(r.status().as_u16(), 201);
    assert_eq!(r.headers().get("x-up").unwrap().to_str().unwrap(), "1");
    assert_eq!(r.text().await.unwrap(), "resp-body");

    // Upstream got the raw bytes, the query, the method, and content-type.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body.as_slice(), raw, "raw body byte-identical");
    assert_eq!(requests[0].url.query(), Some("foo=bar"), "query preserved");
    assert_eq!(requests[0].method.as_str(), "POST", "method preserved");
    assert_eq!(
        requests[0]
            .headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/x-raw-test",
        "content-type preserved"
    );
}

#[tokio::test]
async fn get_models_passes_through_catch_all() {
    const MODELS: &str = r#"{"data":[{"id":"m"}]}"#;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(MODELS),
        )
        .mount(&server)
        .await;
    let proxy = spawn_proxy(rules_cfg(&format!("{}/v1", server.uri()))).await;
    let client = Client::new();
    let r = client
        .get(format!("{proxy}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.text().await.unwrap(), MODELS);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method.as_str(), "GET");
    assert!(requests[0].body.is_empty(), "GET forwards with no body");
}

#[tokio::test]
async fn sse_stream_chunks_pass_through_byte_identical() {
    const SSE: &str = "data: {\"a\":1}\n\ndata: [DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        // set_body_raw sets the content-type directly (set_body_string would
        // reset it to text/plain).
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE.as_bytes(), "text/event-stream"))
        .mount(&server)
        .await;
    let proxy = spawn_proxy(rules_cfg(&format!("{}/v1", server.uri()))).await;
    let client = Client::new();
    let r = client
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&serde_json::json!({
            "stream": true,
            "messages": [{"role": "user", "content": "hello world here friend"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        r.headers().get("content-type").unwrap().to_str().unwrap(),
        "text/event-stream",
        "SSE content-type preserved"
    );
    assert_eq!(r.text().await.unwrap(), SSE, "SSE body byte-identical");
}

#[tokio::test]
async fn streaming_chunks_are_delivered_incrementally() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const CHUNK1: &str = "data: {\"a\":1}\n\n";
    const CHUNK2: &str = "data: [DONE]\n\n";

    // Progressive delivery is pinned by CAUSALITY, not a wall-clock race
    // (loopback ACK-timing quirks make sub-second latency assertions flaky):
    // the hand-rolled upstream (wiremock can't pace chunks) writes the head +
    // CHUNK1, then refuses to send CHUNK2/EOF until the client has observed
    // CHUNK1 THROUGH the proxy. A proxy that buffers the upstream body
    // deadlocks — it won't release CHUNK1 before EOF, and EOF won't happen
    // before CHUNK1 is seen — so the hard timeout below fails it.
    let (got_first_tx, got_first_rx) = tokio::sync::oneshot::channel::<()>();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_base = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        sock.set_nodelay(true).unwrap();
        // Drain the proxy's full request: headers + content-length body.
        let mut data = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            assert!(n > 0, "proxy closed before sending a full request");
            data.extend_from_slice(&buf[..n]);
            if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..pos]).to_string();
                let content_length = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        if k.eq_ignore_ascii_case("content-length") {
                            v.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                if data.len() >= pos + 4 + content_length {
                    break;
                }
            }
        }
        // No content-length/transfer-encoding: the body is EOF-terminated,
        // so nothing downstream can know the size and pre-buffer honestly.
        sock.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        sock.write_all(CHUNK1.as_bytes()).await.unwrap();
        sock.flush().await.unwrap();
        // Hold the stream open until the client proves CHUNK1 got through.
        got_first_rx.await.unwrap();
        sock.write_all(CHUNK2.as_bytes()).await.unwrap();
        sock.flush().await.unwrap();
        // dropping the socket sends EOF, terminating the body
    });

    let proxy = spawn_proxy(rules_cfg(&upstream_base)).await;
    let client = Client::new();
    let received = tokio::time::timeout(std::time::Duration::from_secs(5), async move {
        let mut resp = client
            .post(format!("{proxy}/v1/chat/completions"))
            .json(&serde_json::json!({
                "stream": true,
                "messages": [{"role": "user", "content": "hello world here friend"}],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        // First chunk received while the upstream stream is still open —
        // release the rest of the stream only now.
        let first = resp.chunk().await.unwrap().expect("first chunk");
        got_first_tx.send(()).unwrap();
        let mut received = first.to_vec();
        while let Some(chunk) = resp.chunk().await.unwrap() {
            received.extend_from_slice(&chunk);
        }
        received
    })
    .await
    .expect("timed out waiting for the first chunk: the proxy buffers the stream");

    assert_eq!(
        String::from_utf8(received).unwrap(),
        format!("{CHUNK1}{CHUNK2}"),
        "full stream must be byte-identical"
    );
}

#[tokio::test]
async fn health_reports_without_blocking() {
    let mut cfg = AppConfig::default();
    cfg.local.base_url = "http://127.0.0.1:1/v1".into(); // unreachable local LLM
    cfg.cache.persist = false; // hermetic: no real cache dir in tests
    let proxy = spawn_proxy(cfg).await;
    let client = Client::new();

    let fut = client.get(format!("{proxy}/health")).send();
    let resp = tokio::time::timeout(std::time::Duration::from_secs(4), fut)
        .await
        .expect("health must return without blocking")
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["local"]["ok"], false, "unreachable local LLM");
    assert!(
        body["local"]["model_present"].is_null(),
        "model_present is null when the listing is unavailable"
    );
    assert_eq!(body["config_source"], "test-config");
    assert!(body["cache_entries"].is_number());
    // Persistence is off in this test: the disk tier fields report absence.
    assert!(
        body["cache_disk_entries"].is_null(),
        "disk entries null when persistence is off"
    );
    assert!(
        body["cache_path"].is_null(),
        "cache_path null when persistence is off"
    );
    // Default config keeps keep_alive on ("60m") — /health surfaces it.
    assert_eq!(body["local"]["keep_alive"], "60m");
}

#[tokio::test]
async fn health_omits_keep_alive_when_disabled() {
    let mut cfg = AppConfig::default();
    cfg.local.base_url = "http://127.0.0.1:1/v1".into();
    cfg.local.keep_alive = String::new(); // pinning disabled
    cfg.cache.persist = false;
    let proxy = spawn_proxy(cfg).await;
    let client = Client::new();
    let resp = client.get(format!("{proxy}/health")).send().await.unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["local"]["keep_alive"].is_null());
}

// --- Task 3: savings telemetry ----------------------------------------------

#[tokio::test]
async fn health_totals_accumulate() {
    // Upstream body carries an OpenAI usage block with a cached-token count; the
    // tail inspector must capture it without altering the forwarded bytes.
    const RESP: &str = r#"{"id":"chatcmpl-x","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"done"}}],"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":40}}}"#;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(RESP),
        )
        .mount(&server)
        .await;

    let proxy = spawn_proxy(rules_cfg(&format!("{}/v1", server.uri()))).await;
    let client = Client::new();
    for _ in 0..2 {
        let r = client
            .post(format!("{proxy}/v1/chat/completions"))
            .json(&serde_json::json!({"messages": [{"role": "user", "content": FLUFFY}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        // Drain the full body so the inspector reaches stream-end and folds in
        // the cached-token count before we read /health.
        assert_eq!(
            r.text().await.unwrap(),
            RESP,
            "body forwarded byte-identical"
        );
    }

    let health: serde_json::Value = client
        .get(format!("{proxy}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let t = &health["totals"];
    assert_eq!(t["requests"], 2, "two compressed requests counted");
    assert!(
        t["saved_tokens"].as_u64().unwrap() > 0,
        "rules compression saved tokens"
    );
    assert!(t["before_tokens"].as_u64().unwrap() > t["after_tokens"].as_u64().unwrap());
    assert_eq!(
        t["upstream_cached_tokens"], 80,
        "40 cached tokens × 2 responses"
    );
    assert_eq!(t["responses_with_cache_info"], 2);
    assert!(t["since"].as_str().unwrap().ends_with('Z'), "RFC3339 since");
    // Field-name pin: the USD key matches stats.rs' `usd_saved_est` (one name
    // across every JSON surface — encode --json and /health totals).
    assert!(
        t["usd_saved_est"].as_f64().unwrap() > 0.0,
        "usd_saved_est present under the standardized name"
    );
}

#[tokio::test]
async fn totals_survive_restart() {
    const RESP: &str = r#"{"choices":[{"message":{"role":"assistant","content":"done"}}]}"#;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RESP))
        .mount(&server)
        .await;

    // Persist ON, on an isolated tempdir cache path (never the user's real dir).
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = rules_cfg(&format!("{}/v1", server.uri()));
    cfg.cache.persist = true;
    cfg.cache.path = Some(dir.path().join("rewrites.sqlite3"));

    // Proxy A: one compressed request, then GET /health to flush totals to disk.
    let proxy_a = spawn_proxy(cfg.clone()).await;
    let client = Client::new();
    let r = client
        .post(format!("{proxy_a}/v1/chat/completions"))
        .json(&serde_json::json!({"messages": [{"role": "user", "content": FLUFFY}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let _ = r.text().await.unwrap();
    let health_a: serde_json::Value = client
        .get(format!("{proxy_a}/health")) // /health flushes totals_json
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health_a["totals"]["requests"], 1);

    // Proxy B: fresh process state, SAME cache path — totals carry over.
    let proxy_b = spawn_proxy(cfg).await;
    let health_b: serde_json::Value = client
        .get(format!("{proxy_b}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        health_b["totals"]["requests"].as_u64().unwrap() >= 1,
        "restarted proxy carried over the prior request count"
    );
    // `since` is the ORIGINAL creation stamp, preserved across the restart.
    assert_eq!(
        health_b["totals"]["since"], health_a["totals"]["since"],
        "since is preserved across restart, not reset"
    );
}

/// Drive `n` compressed chat requests through a persist=true proxy on a fresh
/// tempdir — deliberately NEVER reading /health on it (that would flush) — then
/// spawn a second proxy on the same path and report the request count its
/// totals carried over. Isolates the every-32-requests flush trigger.
async fn requests_carried_after_restart(upstream_base: &str, n: usize) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = rules_cfg(upstream_base);
    cfg.cache.persist = true;
    cfg.cache.path = Some(dir.path().join("rewrites.sqlite3"));

    let proxy_a = spawn_proxy(cfg.clone()).await;
    let client = Client::new();
    for _ in 0..n {
        let r = client
            .post(format!("{proxy_a}/v1/chat/completions"))
            .json(&serde_json::json!({"messages": [{"role": "user", "content": FLUFFY}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let _ = r.text().await.unwrap();
    }

    // Restart onto the same DB: whatever was flushed is what carries over.
    let proxy_b = spawn_proxy(cfg).await;
    let health: serde_json::Value = client
        .get(format!("{proxy_b}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    health["totals"]["requests"].as_u64().unwrap()
}

#[tokio::test]
async fn totals_flush_at_32_request_boundary() {
    const RESP: &str = r#"{"choices":[{"message":{"role":"assistant","content":"done"}}]}"#;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RESP))
        .mount(&server)
        .await;
    let base = format!("{}/v1", server.uri());

    // Exactly 32 counted requests: the every-32nd-request flush fires, so the
    // count survives a restart WITHOUT any /health read having happened.
    assert_eq!(
        requests_carried_after_restart(&base, 32).await,
        32,
        "the 32nd counted request must flush totals to disk"
    );

    // Negative control: 31 requests never cross the boundary (and /health was
    // never read), so nothing was flushed — the restarted proxy starts fresh.
    assert_eq!(
        requests_carried_after_restart(&base, 31).await,
        0,
        "below the 32-request boundary nothing is flushed"
    );
}

// --- Task 3b: Anthropic passthrough + draft dashboard -----------------------

#[tokio::test]
async fn anthropic_messages_compressed_and_forwarded() {
    // An Anthropic-shaped upstream reply (content[0].text + a cache-read usage).
    const RESP: &str = r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"PONG"}],"usage":{"input_tokens":10,"cache_read_input_tokens":5,"output_tokens":2}}"#;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(RESP),
        )
        .mount(&server)
        .await;

    // x_api_key auth style + a unique env var (process-global; parallel tests).
    let env_name = "PROMPT_CODEC_TEST_ZAI_KEY_MESSAGES";
    std::env::set_var(env_name, "glm-secret-key");
    let mut cfg = rules_cfg(&format!("{}/v1", server.uri()));
    cfg.proxy.upstream_auth_style = "x_api_key".into();
    cfg.proxy.upstream_api_key_env = env_name.to_string();
    let proxy = spawn_proxy(cfg).await;

    // The tool_result block must survive byte-identical (never traversed).
    let tool_result = serde_json::json!({
        "type": "tool_result",
        "tool_use_id": "t1",
        "content": "data"
    });
    let client = Client::new();
    let r = client
        .post(format!("{proxy}/v1/messages"))
        .json(&serde_json::json!({
            "model": "glm-5.2",
            "max_tokens": 64,
            "system": "keep me",
            "messages": [
                {"role": "user", "content": FLUFFY},
                {"role": "assistant", "content": "Sure, I can help with that."},
                {"role": "user", "content": [
                    tool_result,
                    {"type": "text", "text": FLUFFY}
                ]}
            ],
        }))
        .send()
        .await
        .unwrap();

    // Client sees the upstream body verbatim + our stat headers.
    assert_eq!(r.status().as_u16(), 200);
    assert!(r.headers().contains_key("x-prompt-codec-before"));
    assert!(r.headers().contains_key("x-prompt-codec-after"));
    assert!(r.headers().contains_key("x-prompt-codec-saved-pct"));
    assert_eq!(
        r.text().await.unwrap(),
        RESP,
        "upstream body byte-identical"
    );

    // Upstream saw compressed text, an untouched tool_result + system, and the
    // Anthropic-style auth/version headers.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert_eq!(
        req.headers.get("x-api-key").unwrap().to_str().unwrap(),
        "glm-secret-key",
        "env key sent as x-api-key"
    );
    assert!(
        req.headers.contains_key("anthropic-version"),
        "anthropic-version header forwarded"
    );

    let sent: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    let m0 = sent["messages"][0]["content"].as_str().unwrap();
    assert!(!m0.contains("Thank you"), "string user content compressed");
    assert!(m0.contains("src/auth.rs"), "real content preserved");

    // tool_result block: deep-equal to the original (byte-identical structure).
    assert_eq!(
        sent["messages"][2]["content"][0], tool_result,
        "tool_result block passed through untouched"
    );
    let text_block = sent["messages"][2]["content"][1]["text"].as_str().unwrap();
    assert!(
        !text_block.contains("Thank you"),
        "text content block compressed"
    );
    assert!(text_block.contains("src/auth.rs"), "real content preserved");

    // Top-level `system` is left exactly as sent.
    assert_eq!(sent["system"], "keep me", "system field untouched");
}

#[tokio::test]
async fn dashboard_serves_html_and_data() {
    const RESP: &str = r#"{"choices":[{"message":{"role":"assistant","content":"done"}}]}"#;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RESP))
        .mount(&server)
        .await;

    let proxy = spawn_proxy(rules_cfg(&format!("{}/v1", server.uri()))).await;
    let client = Client::new();

    // The page is self-contained HTML naming the product.
    let page = client
        .get(format!("{proxy}/dashboard"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status().as_u16(), 200);
    assert!(
        page.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/html"),
        "dashboard is served as HTML"
    );
    assert!(page.text().await.unwrap().contains("prompt-codec"));

    // One compressed request, then the poll feed reflects it.
    let r = client
        .post(format!("{proxy}/v1/chat/completions"))
        .json(&serde_json::json!({"messages": [{"role": "user", "content": FLUFFY}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let _ = r.text().await.unwrap();

    let data: serde_json::Value = client
        .get(format!("{proxy}/dashboard/data"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(data["totals"]["requests"], 1);
    assert!(data["totals"]["usd_saved_est"].is_number());
    let recent = data["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 1, "one recent request recorded");
    assert_eq!(recent[0]["endpoint"], "/v1/chat/completions");
    assert!(recent[0]["before_tokens"].as_u64().unwrap() > 0);
    assert!(recent[0]["after_tokens"].as_u64().unwrap() > 0);
    assert!(
        recent[0]["before_tokens"].as_u64().unwrap() > recent[0]["after_tokens"].as_u64().unwrap(),
        "rules compression saved tokens"
    );
}

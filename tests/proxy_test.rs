// Config is built via `Default` + field assignment throughout these tests;
// the resulting style lint is noise on test scaffolding, not the assertions.
#![allow(clippy::field_reassign_with_default)]

use prompt_codec::config::AppConfig;
use reqwest::Client;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Spawn the proxy on an ephemeral loopback port and return its base URL.
async fn spawn_proxy(cfg: AppConfig) -> String {
    let app = prompt_codec::proxy::create_app(cfg, "test-config".into());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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
    assert!(body["error"]["message"].is_string());
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
async fn health_reports_without_blocking() {
    let mut cfg = AppConfig::default();
    cfg.local.base_url = "http://127.0.0.1:1/v1".into(); // unreachable local LLM
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
}

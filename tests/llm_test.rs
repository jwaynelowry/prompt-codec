// Config is built via `Default` + field assignment throughout these tests;
// the resulting style lint is noise on test scaffolding, not the assertions.
#![allow(clippy::field_reassign_with_default)]

use prompt_codec::config::LocalConfig;
use prompt_codec::llm::LlmClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn cfg(base: &str, timeout_s: f64) -> (LocalConfig, f64) {
    let mut c = LocalConfig::default();
    c.base_url = format!("{base}/v1");
    (c, timeout_s)
}

#[tokio::test]
async fn encode_text_returns_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "short version"}, "finish_reason": "stop"}]
        })))
        .mount(&server)
        .await;
    let (c, t) = cfg(&server.uri(), 5.0);
    let llm = LlmClient::new(&c, t);
    let out = llm.encode_text("some long prompt", 0.45).await.unwrap();
    assert_eq!(out, "short version");
}

#[tokio::test]
async fn timeout_is_an_error_not_a_hang() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(5))
                .set_body_json(serde_json::json!({"choices": []})),
        )
        .mount(&server)
        .await;
    let (c, t) = cfg(&server.uri(), 0.2);
    let llm = LlmClient::new(&c, t);
    let start = std::time::Instant::now();
    assert!(llm.encode_text("x", 0.45).await.is_err());
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

#[tokio::test]
async fn reasoning_effort_sent_by_default_and_omitted_when_empty() {
    use wiremock::matchers::body_partial_json;
    // Default config → reasoning_effort: "none" must be in the request body.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({"reasoning_effort": "none"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "compressed output text"}, "finish_reason": "stop"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (c, t) = cfg(&server.uri(), 5.0);
    LlmClient::new(&c, t).encode_text("x", 0.45).await.unwrap();

    // Empty setting → the field must be absent entirely.
    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "compressed output text"}, "finish_reason": "stop"}]
        })))
        .mount(&server2)
        .await;
    let (mut c2, t2) = cfg(&server2.uri(), 5.0);
    c2.reasoning_effort = String::new();
    LlmClient::new(&c2, t2)
        .encode_text("x", 0.45)
        .await
        .unwrap();
    let sent = &server2.received_requests().await.unwrap()[0];
    let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert!(body.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn truncated_output_is_rejected() {
    // finish_reason=length must be an error — a truncated rewrite always "saves
    // tokens" and would otherwise be forwarded with its tail silently dropped.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "cut off mid"}, "finish_reason": "length"}]
        })))
        .mount(&server)
        .await;
    let (c, t) = cfg(&server.uri(), 5.0);
    assert!(LlmClient::new(&c, t)
        .encode_text("long prompt", 0.45)
        .await
        .is_err());
}

#[tokio::test]
async fn health_model_present_is_none_for_unexpected_body_shape() {
    // A reachable server whose /models body is not an OpenAI listing (no
    // "data" array) must report model_present: None — informational unknown,
    // never a false Some(false).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"error": "x"})))
        .mount(&server)
        .await;
    let (c, t) = cfg(&server.uri(), 5.0);
    let h = LlmClient::new(&c, t).health().await;
    assert!(h.ok); // status 200 — the server itself is fine
    assert_eq!(h.model_present, None);
}

#[tokio::test]
async fn health_data_null_is_an_empty_listing() {
    // Ollama with zero pulled models returns {"data": null}: a real, reachable
    // listing that simply has no models -> model_present Some(false), so the
    // proxy can warn that hybrid will degrade to rules-only.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": null})))
        .mount(&server)
        .await;
    let (c, t) = cfg(&server.uri(), 5.0);
    let h = LlmClient::new(&c, t).health().await;
    assert!(h.ok);
    assert_eq!(h.model_present, Some(false));
}

#[tokio::test]
async fn invalid_timeout_values_do_not_panic() {
    // A hand-edited YAML can hold NaN/negative/overflowing timeout values;
    // client construction must clamp instead of panicking at startup.
    for bad in [f64::NAN, f64::INFINITY, -3.0, 0.0, 1e300] {
        let _ = LlmClient::new(&LocalConfig::default(), bad);
    }
}

#[tokio::test]
async fn health_probe_reports_ok_model_presence_and_down() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "gemma4:12b-mlx"}]})))
        .mount(&server)
        .await;
    let (c, t) = cfg(&server.uri(), 5.0);
    let h = LlmClient::new(&c, t).health().await;
    assert!(h.ok);
    assert_eq!(h.model_present, Some(true)); // default model is gemma4:12b-mlx
    let mut down = LocalConfig::default();
    down.base_url = "http://127.0.0.1:1/v1".into();
    assert!(!LlmClient::new(&down, 5.0).health().await.ok);
}

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
async fn health_probe_reports_ok_model_presence_and_down() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "gemma3:4b"}]})))
        .mount(&server)
        .await;
    let (c, t) = cfg(&server.uri(), 5.0);
    let h = LlmClient::new(&c, t).health().await;
    assert!(h.ok);
    assert_eq!(h.model_present, Some(true)); // default model is gemma3:4b
    let mut down = LocalConfig::default();
    down.base_url = "http://127.0.0.1:1/v1".into();
    assert!(!LlmClient::new(&down, 5.0).health().await.ok);
}

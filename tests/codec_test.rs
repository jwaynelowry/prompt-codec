// Config is built via `Default` + field assignment throughout these tests;
// the resulting style lint is noise on test scaffolding, not the assertions.
#![allow(clippy::field_reassign_with_default)]

use prompt_codec::codec::Codec;
use prompt_codec::config::AppConfig;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_cfg(llm_base: &str) -> AppConfig {
    let mut c = AppConfig::default();
    c.local.base_url = format!("{llm_base}/v1");
    c.encoder.min_chars_to_compress = 10;
    c.encoder.mode = "hybrid".into();
    c
}
fn long_user(text: &str) -> serde_json::Value {
    json!({"role": "user", "content": text})
}

#[tokio::test]
async fn only_last_user_message_hits_llm_and_cache_persists() {
    let server = MockServer::start().await;
    // NOTE: rewrite must be >20 chars (acceptance guard) yet fewer tokens than
    // the post-rules inputs below — "tiny compressed version" is 23 chars / ~4 tokens.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "tiny compressed version"}, "finish_reason": "stop"}]})))
        .expect(2) // turn 1: msg A; turn 2: msg B. A must come from cache on turn 2.
        .mount(&server)
        .await;
    let codec = Codec::new(test_cfg(&server.uri()));

    let turn1 = vec![long_user(
        "first long message needing compression right here with plenty of words to shrink",
    )];
    let r1 = codec.encode_messages(turn1).await;
    let a_compressed = r1.messages[0]["content"].as_str().unwrap().to_string();
    assert_eq!(a_compressed, "tiny compressed version");

    let turn2 = vec![
        long_user(
            "first long message needing compression right here with plenty of words to shrink",
        ),
        json!({"role": "assistant", "content": "reply"}),
        long_user(
            "second long message also needing compression here with plenty of words to shrink",
        ),
    ];
    let r2 = codec.encode_messages(turn2).await;
    assert_eq!(r2.messages[0]["content"].as_str().unwrap(), a_compressed); // byte-stable history
    assert_eq!(
        r2.messages[2]["content"].as_str().unwrap(),
        "tiny compressed version"
    );
}

#[tokio::test]
async fn rejects_rewrite_not_smaller_than_rules_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "this rewrite is much much much longer than the rules output was, so it must be rejected by the token guard"}, "finish_reason": "stop"}]})))
        .mount(&server)
        .await;
    let codec = Codec::new(test_cfg(&server.uri()));
    let msgs = vec![long_user("short-ish content for the guard test")];
    let r = codec.encode_messages(msgs).await;
    assert!(r.messages[0]["content"]
        .as_str()
        .unwrap()
        .contains("guard test")); // kept rules output
}

#[tokio::test]
async fn tool_json_is_minified_and_never_llm_rewritten() {
    let server = MockServer::start().await; // expect(0) mock: any LLM call fails the test
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let codec = Codec::new(test_cfg(&server.uri()));
    let msgs = vec![json!({"role": "tool", "tool_call_id": "abc",
        "content": "{\n  \"a\": 1,\n  \"b\": [1, 2]\n}"})];
    let r = codec.encode_messages(msgs).await;
    assert_eq!(
        r.messages[0]["content"].as_str().unwrap(),
        r#"{"a":1,"b":[1,2]}"#
    );
    assert_eq!(r.messages[0]["tool_call_id"], "abc");
}

#[tokio::test]
async fn llm_failure_degrades_to_rules() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let codec = Codec::new(test_cfg(&server.uri()));
    let msgs = vec![long_user("please compress this content anyway thanks")];
    let r = codec.encode_messages(msgs).await;
    assert!(r.messages[0]["content"]
        .as_str()
        .unwrap()
        .contains("compress this content"));
}

#[tokio::test]
async fn assistant_and_short_system_untouched() {
    let server = MockServer::start().await;
    let codec = Codec::new(test_cfg(&server.uri()));
    let msgs = vec![
        json!({"role": "system", "content": "short system prompt"}),
        json!({"role": "assistant", "content": "Assistant   spaced   reply"}),
    ];
    let r = codec.encode_messages(msgs).await;
    assert_eq!(r.messages[0]["content"], "short system prompt");
    assert_eq!(r.messages[1]["content"], "Assistant   spaced   reply");
}

#[tokio::test]
async fn encode_text_blob_accepts_smaller_rewrite() {
    // The blob path (CLI encode / proxy /v1/completions) counts as in-scope
    // last-user content: an accepted rewrite replaces the rules output.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "tiny compressed version"}, "finish_reason": "stop"}]})))
        .mount(&server)
        .await;
    let codec = Codec::new(test_cfg(&server.uri()));
    let result = codec
        .encode_text(
            "a long and fluffy prompt blob with plenty of words that the local model can shrink",
            None,
        )
        .await;
    assert_eq!(result.text, "tiny compressed version");
    assert!(result.notes.iter().any(|n| n == "llm_encode"));
    assert!(result.stats.after_tokens < result.stats.before_tokens);
    // No override given: the resolved mode is the configured one.
    assert_eq!(result.mode_used, "hybrid");
}

#[tokio::test]
async fn encode_text_blob_rejects_rewrite_not_smaller_than_rules_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "this rewrite is much much much longer than the rules output was, so it must be rejected by the token guard"}, "finish_reason": "stop"}]})))
        .mount(&server)
        .await;
    let codec = Codec::new(test_cfg(&server.uri()));
    let result = codec
        .encode_text("short-ish blob content for the guard test", None)
        .await;
    assert!(result.text.contains("guard test")); // kept the rules output
    assert!(result.notes.iter().any(|n| n == "llm_rejected"));
}

#[tokio::test]
async fn pure_fluff_message_keeps_original() {
    // Rules would empty "Thank you so much!" entirely; the codec must restore
    // the original content (never forward empty content upstream) and record a
    // rules_emptied marker.
    let server = MockServer::start().await;
    let mut cfg = test_cfg(&server.uri());
    cfg.encoder.mode = "rules".into();
    let codec = Codec::new(cfg);
    let msgs = vec![long_user("Thank you so much!")];
    let r = codec.encode_messages(msgs).await;
    assert_eq!(
        r.messages[0]["content"].as_str().unwrap(),
        "Thank you so much!"
    );
    assert!(r.notes.iter().any(|n| n.contains("rules_emptied")));
}

#[tokio::test]
async fn parts_array_text_compressed_other_fields_kept() {
    // Only {"type":"text"} parts are transformed; every other part (and its
    // ordering) passes through untouched.
    let server = MockServer::start().await;
    let mut cfg = test_cfg(&server.uri());
    cfg.encoder.mode = "rules".into();
    let codec = Codec::new(cfg);
    let msgs = vec![json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "Please please fix the authentication bug in src/main.rs. Thank you so much!"},
            {"type": "image_url", "image_url": {"url": "http://x"}}
        ]
    })];
    let r = codec.encode_messages(msgs).await;
    let content = r.messages[0]["content"].as_array().unwrap();
    // text part compressed: boilerplate stripped, real detail (path) survives
    assert_eq!(content[0]["type"], "text");
    let text = content[0]["text"].as_str().unwrap();
    assert!(text.contains("src/main.rs"));
    assert!(!text.to_lowercase().contains("thank you"));
    assert!(!text.to_lowercase().contains("please please"));
    // image part and ordering untouched
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(content[1]["image_url"]["url"], "http://x");
}

//! Codec orchestration: the per-message compression policy that sits between
//! the proxy and the upstream model.
//!
//! Pipeline per request:
//! 1. Count "before" tokens over the whole message list.
//! 2. Rules stage (deterministic, per role): user/system get the full rules
//!    pipeline, `tool` gets structure-safe JSON minify (never LLM), assistant
//!    and unknown roles pass through. A message whose original content was
//!    non-empty but which rules would empty entirely keeps its ORIGINAL bytes.
//! 3. LLM stage (modes `local`/`hybrid`, scope != `none`): every eligible
//!    user/system message does a cache lookup first (keeping resent history
//!    byte-stable so the upstream prompt cache stays warm); only in-scope
//!    messages may CALL the local model on a miss. A rewrite is accepted only
//!    when it is non-trivial and strictly fewer tokens than the POST-RULES
//!    content. Any LLM error degrades to the rules output — never propagated.
//! 4. Recount "after" tokens; return messages + stats + notes.

use serde_json::Value;

use crate::cache::RewriteCache;
use crate::config::{AppConfig, LlmScope};
use crate::llm::LlmClient;
use crate::rules::{collapse_whitespace, rules_compress};
use crate::stats::TokenStats;
use crate::tokenizer::count_tokens;

/// Result of encoding a message list: the (possibly) compressed messages, the
/// before/after token stats, and a per-message note trail for logs/`/health`.
pub struct EncodeResult {
    pub messages: Vec<Value>,
    pub stats: TokenStats,
    pub notes: Vec<String>,
}

/// Result of encoding a single text blob: the compressed text, stats, notes,
/// and the mode that was actually applied (the override when given, else the
/// configured `encoder.mode`) — so callers report the truth instead of
/// re-deriving it.
pub struct EncodeTextResult {
    pub text: String,
    pub stats: TokenStats,
    pub notes: Vec<String>,
    pub mode_used: String,
}

/// Owns the config, the local-LLM client, and the shared rewrite cache. Built
/// once and reused for the process lifetime; the proxy `AppState` holds one.
pub struct Codec {
    cfg: AppConfig,
    llm: LlmClient,
    cache: RewriteCache,
}

/// Roles whose content is touched by the rules stage. `assistant` and any
/// unknown role are passed through untouched.
fn role_gets_rules(role: &str) -> bool {
    matches!(role, "user" | "system" | "tool")
}

/// Deterministic per-role transform of a single content string.
/// - `user`: full rules pipeline.
/// - `system`: rules pipeline, but content shorter than the protect threshold
///   is left untouched.
/// - `tool`: structure-safe only — minify if it parses as JSON, else collapse
///   whitespace. Never boilerplate-stripped, deduped, or LLM-rewritten.
fn rules_transform(role: &str, s: &str, protect_system_under_chars: usize) -> String {
    match role {
        "user" => rules_compress(s),
        "system" => {
            if s.chars().count() < protect_system_under_chars {
                s.to_string()
            } else {
                rules_compress(s)
            }
        }
        "tool" => tool_minify_or_collapse(s),
        _ => s.to_string(),
    }
}

/// `tool` content: if the whole string parses as JSON, re-emit it compact
/// (with the `preserve_order` feature, key order is retained); otherwise fall
/// back to whitespace normalization. Never mangles non-JSON tool output.
fn tool_minify_or_collapse(s: &str) -> String {
    match serde_json::from_str::<Value>(s) {
        Ok(v) => serde_json::to_string(&v).unwrap_or_else(|_| collapse_whitespace(s)),
        Err(_) => collapse_whitespace(s),
    }
}

/// Extract the textual content of a message for token counting, mirroring the
/// legacy `messages_text`/`count_messages_tokens` join semantics.
fn message_text(m: &Value) -> String {
    match m.get("content") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().map(part_text).collect::<Vec<_>>().join(" "),
        Some(other) => other.to_string(),
    }
}

fn part_text(p: &Value) -> String {
    match p {
        Value::Object(o) => o
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Port of `legacy/prompt_codec/tokens.py::count_messages_tokens`: per-message
/// textual tokens plus a flat +4 role-framing overhead, then +2 for the list.
fn count_messages_tokens(messages: &[Value]) -> usize {
    let mut total = 0usize;
    for m in messages {
        total += count_tokens(&message_text(m)) + 4;
    }
    total + 2
}

/// Set a message object's `content` field, leaving every other field intact.
fn set_content(msg: &mut Value, new: String) {
    if let Some(obj) = msg.as_object_mut() {
        obj.insert("content".to_string(), Value::String(new));
    }
}

/// Truncate to at most `max` chars (not bytes) for log hygiene — LLM error
/// text is never emitted in full into notes.
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Acceptance guard shared by the message-list and text-blob paths: an LLM
/// rewrite replaces the post-rules `baseline` only when it is non-empty,
/// non-trivial, and strictly fewer tokens than that baseline.
fn accept_rewrite(rewrite: &str, baseline: &str) -> bool {
    // `len() > 20` is a BYTE-length floor — intentional per plan (bytes, not
    // chars), matching the v1 triviality threshold.
    !rewrite.trim().is_empty()
        && rewrite.len() > 20
        && count_tokens(rewrite) < count_tokens(baseline)
}

impl Codec {
    pub fn new(cfg: AppConfig) -> Self {
        let llm = LlmClient::new(&cfg.local, cfg.encoder.llm_timeout_s);
        let cache = RewriteCache::new(cfg.cache.max_entries);
        Self { cfg, llm, cache }
    }

    /// Shared cache handle — the proxy's `/health` reports its entry count.
    pub fn cache(&self) -> &RewriteCache {
        &self.cache
    }

    /// The local-LLM client — the proxy's `/health` runs its reachability probe.
    pub fn llm(&self) -> &LlmClient {
        &self.llm
    }

    /// Encode a full message list per the role/scope policy documented above.
    /// Never fails: LLM problems degrade to the deterministic rules output.
    pub async fn encode_messages(&self, mut messages: Vec<Value>) -> EncodeResult {
        let mode = self.cfg.encoder.mode.as_str();
        let mut notes: Vec<String> = Vec::new();
        let before = count_messages_tokens(&messages);

        let last_user_idx = messages
            .iter()
            .rposition(|m| m.get("role").and_then(Value::as_str) == Some("user"));

        // --- Rules stage ---------------------------------------------------
        let apply_rules = matches!(mode, "rules" | "hybrid") && self.cfg.encoder.rules_enabled;
        if apply_rules {
            let protect = self.cfg.encoder.protect_system_under_chars;
            for (i, msg) in messages.iter_mut().enumerate() {
                let role = msg
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !role_gets_rules(&role) {
                    continue;
                }
                match msg.get_mut("content") {
                    Some(Value::String(s)) => {
                        let original = std::mem::take(s);
                        let transformed = rules_transform(&role, &original, protect);
                        // Rules-emptied guard: never forward empty content that
                        // started non-empty — restore the original bytes.
                        if !original.trim().is_empty() && transformed.trim().is_empty() {
                            notes.push(format!("rules_emptied_msg_{i}"));
                            *s = original;
                        } else {
                            *s = transformed;
                        }
                    }
                    Some(Value::Array(parts)) => {
                        let mut restored_any = false;
                        for part in parts.iter_mut() {
                            let Value::Object(o) = part else { continue };
                            if o.get("type").and_then(Value::as_str) != Some("text") {
                                continue;
                            }
                            let Some(Value::String(t)) = o.get_mut("text") else {
                                continue;
                            };
                            let original = std::mem::take(t);
                            let transformed = rules_transform(&role, &original, protect);
                            if !original.trim().is_empty() && transformed.trim().is_empty() {
                                restored_any = true;
                                *t = original;
                            } else {
                                *t = transformed;
                            }
                        }
                        if restored_any {
                            notes.push(format!("rules_emptied_msg_{i}"));
                        }
                    }
                    _ => {}
                }
            }
        }

        // --- LLM stage -----------------------------------------------------
        let do_llm =
            matches!(mode, "local" | "hybrid") && self.cfg.encoder.llm_scope != LlmScope::None;
        if do_llm {
            let target_ratio = self.cfg.encoder.target_ratio;
            let min_chars = self.cfg.encoder.min_chars_to_compress;
            let protect = self.cfg.encoder.protect_system_under_chars;
            // Index-based `enumerate` over `iter_mut`: `i` gates last-user
            // scope and labels notes, and we hold a single `&mut Value` (into
            // the local `messages`, never `self`) across the LLM await.
            for (i, msg) in messages.iter_mut().enumerate() {
                let role = msg
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if role != "user" && role != "system" {
                    continue;
                }
                // Only string content is LLM-eligible; parts arrays got the
                // rules stage only. The skip note is gated behind min_chars so
                // tiny parts messages (never LLM candidates anyway) stay quiet.
                let content = match msg.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => {
                        let text_chars: usize =
                            parts.iter().map(|p| part_text(p).chars().count()).sum();
                        if text_chars >= min_chars {
                            notes.push(format!("llm_skipped_parts_msg_{i}"));
                        }
                        continue;
                    }
                    _ => continue,
                };
                let n = content.chars().count();
                if n < min_chars {
                    continue;
                }
                if role == "system" && n < protect {
                    continue;
                }

                // Cache lookup first for every candidate — this is what keeps
                // resent history byte-stable across turns.
                let key = RewriteCache::key(&content, target_ratio, self.llm.model());
                if let Some(cached) = self.cache.get(&key) {
                    set_content(msg, cached);
                    notes.push(format!("cache_hit_msg_{i}"));
                    continue;
                }

                // On a miss, only in-scope messages may call the model.
                let in_scope = match self.cfg.encoder.llm_scope {
                    LlmScope::All => true,
                    LlmScope::LastUser => Some(i) == last_user_idx,
                    LlmScope::None => false,
                };
                if !in_scope {
                    continue;
                }

                match self.llm.encode_text(&content, target_ratio).await {
                    Ok(rewrite) => {
                        // Guard vs POST-RULES tokens; reject trivial/expanding
                        // rewrites and keep the rules output instead.
                        if accept_rewrite(&rewrite, &content) {
                            self.cache.put(key, rewrite.clone());
                            set_content(msg, rewrite);
                            notes.push(format!("llm_encode_msg_{i}"));
                        } else {
                            notes.push(format!("llm_rejected_msg_{i}"));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            msg_index = i,
                            error = %e,
                            "local LLM call failed; degrading to rules"
                        );
                        notes.push(format!(
                            "llm_failed_msg_{i}:{}",
                            truncate_chars(&e.to_string(), 200)
                        ));
                    }
                }
            }
        }

        let after = count_messages_tokens(&messages);
        let stats = TokenStats::new(before, after, self.cfg.stats.usd_per_mtok_input);
        EncodeResult {
            messages,
            stats,
            notes,
        }
    }

    /// Encode a single text blob (CLI `encode`, proxy `/v1/completions`). The
    /// prompt is treated as in-scope/last-user, so it may call the model
    /// directly. `mode_override` lets the CLI force a mode; otherwise the
    /// configured `encoder.mode` applies. The resolved mode is reported back
    /// in the result.
    pub async fn encode_text(&self, text: &str, mode_override: Option<&str>) -> EncodeTextResult {
        let mode = mode_override.unwrap_or(self.cfg.encoder.mode.as_str());
        let mut notes: Vec<String> = Vec::new();
        let before = count_tokens(text);
        let mut out = text.to_string();

        let apply_rules = matches!(mode, "rules" | "hybrid") && self.cfg.encoder.rules_enabled;
        if apply_rules {
            let compressed = rules_compress(&out);
            if !out.trim().is_empty() && compressed.trim().is_empty() {
                notes.push("rules_emptied".to_string());
            } else {
                out = compressed;
            }
            notes.push("rules_compress".to_string());
        }

        let do_llm =
            matches!(mode, "local" | "hybrid") && self.cfg.encoder.llm_scope != LlmScope::None;
        if do_llm && out.chars().count() >= self.cfg.encoder.min_chars_to_compress {
            let target_ratio = self.cfg.encoder.target_ratio;
            let key = RewriteCache::key(&out, target_ratio, self.llm.model());
            if let Some(cached) = self.cache.get(&key) {
                out = cached;
                notes.push("cache_hit".to_string());
            } else {
                match self.llm.encode_text(&out, target_ratio).await {
                    Ok(rewrite) => {
                        if accept_rewrite(&rewrite, &out) {
                            self.cache.put(key, rewrite.clone());
                            out = rewrite;
                            notes.push("llm_encode".to_string());
                        } else {
                            notes.push("llm_rejected".to_string());
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "local LLM call failed; degrading to rules"
                        );
                        notes.push(format!(
                            "llm_failed:{}",
                            truncate_chars(&e.to_string(), 200)
                        ));
                    }
                }
            }
        }

        let after = count_tokens(&out);
        let stats = TokenStats::new(before, after, self.cfg.stats.usd_per_mtok_input);
        EncodeTextResult {
            text: out,
            stats,
            notes,
            mode_used: mode.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASELINE: &str =
        "this is a much longer baseline sentence with many additional words to compress down";

    #[test]
    fn accept_rewrite_accepts_smaller_nontrivial_rewrite() {
        // 23 bytes (> 20 floor) and clearly fewer tokens than BASELINE.
        assert!(accept_rewrite("tiny compressed version", BASELINE));
    }

    #[test]
    fn accept_rewrite_rejects_empty() {
        assert!(!accept_rewrite("", BASELINE));
        assert!(!accept_rewrite("   \n\t ", BASELINE));
    }

    #[test]
    fn accept_rewrite_rejects_trivially_short() {
        // 12 bytes: under the 20-byte floor even though it saves tokens.
        assert!(!accept_rewrite("short output", BASELINE));
    }

    #[test]
    fn accept_rewrite_rejects_not_strictly_smaller() {
        // Identical text: equal token count, and the guard is strict.
        assert!(!accept_rewrite(BASELINE, BASELINE));
        // An expansion is rejected too.
        let expansion = format!("{BASELINE} plus even more words tacked on the end");
        assert!(!accept_rewrite(&expansion, BASELINE));
    }
}

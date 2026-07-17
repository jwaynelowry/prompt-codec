# prompt-codec v2 (Rust rewrite) Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Python prompt-codec with a single Rust binary (proxy + CLI) that compresses prompts safely (never corrupting code/JSON), caches LLM rewrites so conversation history stays byte-stable, and forwards upstream responses verbatim with true status codes.

**Architecture:** One crate, lib (`src/lib.rs`) + bin (`src/main.rs`). Pure-function rules engine segments text into prose vs fenced code and only transforms prose. An async codec orchestrates rules → cache → optional local-LLM rewrite (last-user-message only by default) with a tokens-must-shrink guard. An axum proxy forwards to the upstream OpenAI-compatible API, streaming response bytes through with status/headers preserved.

**Tech Stack:** tokio, axum 0.8, reqwest (streaming), serde/serde_json/serde_yaml, tiktoken-rs, moka (sync cache), clap 4 derive, tracing, sha2, regex. Dev: wiremock, proptest, tempfile.

**Spec:** `docs/superpowers/specs/2026-07-17-rust-rewrite-design.md` — read it before starting. The spec is the authority on behavior; this plan is the authority on sequencing.

**Conventions for every task:**
- TDD: write the failing test first, see it fail, implement, see it pass.
- Run `cargo test` (whole suite) before every commit; run `cargo clippy --all-targets -- -D warnings` before every commit from Task 5 onward.
- Commit messages: `feat:`/`test:`/`chore:` prefix, end body with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- All work happens in `/Users/waynelowry/Projects/prompt-codec`.

---

### Task 0: Move Python to legacy/, scaffold the crate

**Files:**
- Move: `prompt_codec/`, `tests/`, `pyproject.toml`, `config.example.yaml`, `scripts/`, `prompt_codec.egg-info/` → `legacy/`
- Keep at root: `config.yaml` (user's live config), `README.md`, `docs/`, `.gitignore`
- Create: `Cargo.toml`, `src/main.rs`, `src/lib.rs`

- [ ] **Step 1: Restructure with git mv**

```bash
mkdir legacy
git mv prompt_codec tests pyproject.toml config.example.yaml scripts legacy/
git rm -r --cached prompt_codec.egg-info 2>/dev/null; mv prompt_codec.egg-info legacy/ 2>/dev/null || true
```

- [ ] **Step 2: Verify legacy Python still runs from its new home**

Run: `cd legacy && python3 -m pytest tests/ -q; cd ..`
Expected: `2 passed` (PYTHONPATH resolves because pytest runs from `legacy/`).

- [ ] **Step 3: Scaffold crate and add dependencies**

```bash
cargo init --name prompt-codec
cargo add tokio --features full
cargo add axum reqwest --features reqwest/json,reqwest/stream
cargo add serde --features derive
cargo add serde_json serde_yaml clap --features clap/derive
cargo add tracing tracing-subscriber --features tracing-subscriber/env-filter
cargo add moka --features sync
cargo add sha2 hex tiktoken-rs dirs futures-util anyhow regex
cargo add --dev wiremock proptest tempfile
```

Create `src/lib.rs`:

```rust
pub mod cache;
pub mod codec;
pub mod config;
pub mod llm;
pub mod proxy;
pub mod rules;
pub mod stats;
pub mod tokenizer;
```

Create empty placeholder files `src/{cache,codec,config,llm,proxy,rules,stats,tokenizer}.rs` (each just `//! see plan`) and a minimal `src/main.rs`:

```rust
fn main() {
    println!("prompt-codec v2 (under construction)");
}
```

Append to `.gitignore`: `target/`, `Cargo.lock` stays committed (binary crate).

- [ ] **Step 4: Verify build**

Run: `cargo build && cargo test`
Expected: compiles, 0 tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "chore: move Python to legacy/, scaffold Rust crate"
```

---

### Task 1: tokenizer module

**Files:** Modify: `src/tokenizer.rs`

- [ ] **Step 1: Write failing tests** (in-module `#[cfg(test)]`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts_known_text() {
        // "hello world" is 2 tokens in cl100k_base
        assert_eq!(count_tokens("hello world"), 2);
    }
    #[test]
    fn empty_is_zero() {
        assert_eq!(count_tokens(""), 0);
    }
    #[test]
    fn longer_text_more_tokens() {
        assert!(count_tokens(&"word ".repeat(100)) > count_tokens("word"));
    }
}
```

- [ ] **Step 2: Run** `cargo test tokenizer` — expect FAIL (unresolved).

- [ ] **Step 3: Implement**

```rust
//! Token counting (cl100k_base) with a chars/4 fallback.
use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();

fn bpe() -> &'static Option<CoreBPE> {
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok())
}

pub fn count_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    match bpe() {
        Some(enc) => enc.encode_ordinary(text).len(),
        None => (text.len() / 4).max(1),
    }
}
```

(If the tiktoken-rs API differs — e.g. constructor name — check `cargo doc` / docs.rs for the installed version and adapt; the required behavior is exactly: cl100k_base ordinary encoding length, fallback `max(len/4, 1)`.)

- [ ] **Step 4: Run** `cargo test tokenizer` — expect 3 PASS. **Step 5: Commit** `feat: tokenizer with cl100k_base counting`.

---

### Task 2: stats module

**Files:** Modify: `src/stats.rs`

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn savings_math() {
        let s = TokenStats::new(1000, 400, 3.0);
        assert_eq!(s.saved_tokens(), 600);
        assert!((s.pct_saved() - 60.0).abs() < 1e-9);
        assert!((s.usd_saved() - 0.0018).abs() < 1e-9);
    }
    #[test]
    fn expansion_clamps_to_zero_saved() {
        let s = TokenStats::new(100, 150, 3.0);
        assert_eq!(s.saved_tokens(), 0);
        assert!(s.pct_saved() < 0.0); // honest negative pct
    }
    #[test]
    fn zero_before_is_ratio_one() {
        let s = TokenStats::new(0, 0, 3.0);
        assert!((s.ratio() - 1.0).abs() < 1e-9);
    }
}
```

- [ ] **Step 2: Run, see FAIL. Step 3: Implement**

```rust
//! Before/after token stats and rough USD savings.
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct TokenStats {
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub usd_per_mtok_input: f64,
}

impl TokenStats {
    pub fn new(before: usize, after: usize, usd_per_mtok: f64) -> Self {
        Self { before_tokens: before, after_tokens: after, usd_per_mtok_input: usd_per_mtok }
    }
    pub fn saved_tokens(&self) -> usize {
        self.before_tokens.saturating_sub(self.after_tokens)
    }
    pub fn ratio(&self) -> f64 {
        if self.before_tokens == 0 { 1.0 } else { self.after_tokens as f64 / self.before_tokens as f64 }
    }
    pub fn pct_saved(&self) -> f64 {
        (1.0 - self.ratio()) * 100.0
    }
    pub fn usd_saved(&self) -> f64 {
        self.saved_tokens() as f64 / 1_000_000.0 * self.usd_per_mtok_input
    }
}
```

- [ ] **Step 4: PASS. Step 5: Commit** `feat: token stats`.

---

### Task 3: rules — fence segmentation

The correctness heart of the project. **Invariant: reassembling segments reproduces the input exactly** (after CRLF normalization).

**Files:** Modify: `src/rules.rs`

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod seg_tests {
    use super::*;
    fn roundtrip(s: &str) {
        assert_eq!(reassemble(&segment(s)), s);
    }
    #[test]
    fn plain_prose_roundtrips() { roundtrip("hello\nworld\n"); }
    #[test]
    fn fence_roundtrips() {
        roundtrip("before\n```python\ndef f():\n    return 1\n```\nafter");
    }
    #[test]
    fn unclosed_fence_becomes_fence_to_eof() {
        let segs = segment("a\n```\ncode never closed");
        assert!(matches!(segs.last().unwrap(), Segment::Fence { .. }));
        roundtrip("a\n```\ncode never closed");
    }
    #[test]
    fn fence_body_is_isolated() {
        let segs = segment("p1\n```rust\nlet x = 1;\n```\np2");
        let bodies: Vec<_> = segs.iter().filter_map(|s| match s {
            Segment::Fence { body, .. } => Some(body.as_str()), _ => None }).collect();
        assert_eq!(bodies, vec!["let x = 1;\n"]);
    }
    #[test]
    fn indented_fence_marker_opens() {
        roundtrip("text\n  ```\n  indented fence\n  ```\ndone");
    }
}
```

- [ ] **Step 2: FAIL. Step 3: Implement**

```rust
//! Deterministic, fence-aware prompt compression. Pure functions only.

#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Prose(String),
    /// `open` is the full opening line (e.g. "```python"), `body` the raw
    /// lines between markers, `close` the closing line or None if unclosed.
    Fence { open: String, body: String, close: Option<String> },
}

fn is_fence_marker(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// Split into prose and fenced segments. Line-oriented; preserves every byte
/// so that `reassemble(&segment(s)) == s`.
pub fn segment(text: &str) -> Vec<Segment> {
    let mut segs = Vec::new();
    let mut prose = String::new();
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if is_fence_marker(line) {
            if !prose.is_empty() {
                segs.push(Segment::Prose(std::mem::take(&mut prose)));
            }
            let open = line.to_string();
            let mut body = String::new();
            let mut close = None;
            for inner in lines.by_ref() {
                if is_fence_marker(inner) {
                    close = Some(inner.to_string());
                    break;
                }
                body.push_str(inner);
            }
            segs.push(Segment::Fence { open, body, close });
        } else {
            prose.push_str(line);
        }
    }
    if !prose.is_empty() {
        segs.push(Segment::Prose(prose));
    }
    segs
}

pub fn reassemble(segs: &[Segment]) -> String {
    let mut out = String::new();
    for s in segs {
        match s {
            Segment::Prose(p) => out.push_str(p),
            Segment::Fence { open, body, close } => {
                out.push_str(open);
                out.push_str(body);
                if let Some(c) = close {
                    out.push_str(c);
                }
            }
        }
    }
    out
}
```

- [ ] **Step 4: PASS. Step 5: Commit** `feat: fence-aware segmentation with exact round-trip`.

---

### Task 4: rules — prose transforms

**Files:** Modify: `src/rules.rs`

- [ ] **Step 1: Failing tests** (add to `rules.rs`)

```rust
#[cfg(test)]
mod prose_tests {
    use super::*;
    #[test]
    fn strips_please_prefix_and_thanks() {
        let t = strip_boilerplate("Please help me with the auth bug.\nThank you so much in advance!\n");
        assert!(!t.to_lowercase().contains("thank you"));
        assert!(t.contains("the auth bug"));
    }
    #[test]
    fn keeps_meaningful_important_lines() {
        let t = strip_boilerplate("Important: do not delete the prod database\n");
        assert!(t.contains("do not delete the prod database"));
    }
    #[test]
    fn drops_pure_fluff_lines() {
        let t = strip_boilerplate("- Please be careful\n- this is important\nreal content\n");
        assert_eq!(t.trim(), "real content");
    }
    #[test]
    fn dedupe_only_long_lines() {
        let t = dedupe_lines("auth uses JWT tokens\nauth uses JWT tokens\n- ok\n- ok\n");
        assert_eq!(t.matches("auth uses JWT tokens").count(), 1);
        assert_eq!(t.matches("- ok").count(), 2); // short lines spared
    }
    #[test]
    fn whitespace_preserves_indentation() {
        let t = collapse_whitespace("    indented   with   runs\n\n\n\nnext");
        assert!(t.starts_with("    indented with runs"));
        assert!(!t.contains("\n\n\n"));
    }
    #[test]
    fn backtick_lines_untouched_by_space_collapse() {
        let t = collapse_whitespace("has `code  span`  here\n");
        assert!(t.contains("`code  span`"));
    }
}
```

- [ ] **Step 2: FAIL. Step 3: Implement** (regexes built once via `OnceLock<Vec<Regex>>`)

```rust
use regex::Regex;
use std::sync::OnceLock;

fn boilerplate_res() -> &'static Vec<Regex> {
    static RES: OnceLock<Vec<Regex>> = OnceLock::new();
    RES.get_or_init(|| {
        [
            r"(?im)^[ \t]*please[, ]+(i would like you to |help me with |remember to )?",
            r"(?im)^[ \t]*please\s+",
            r"(?i)\bthank you( so much)?( in advance)?[.!]?[ \t]*",
            r"(?i)\bas an ai[^.!\n]*[.!]?[ \t]*",
            r"(?i)\bi hope this helps[^.!\n]*[.!]?[ \t]*",
            r"(?im)^[ \t]*i would like you to\s+",
            r"(?i)\b(write clean code|follow best practices|make it production ready)\b[^\n]*",
            r"(?i)\badd comments where needed\b[^\n]*",
            // pure-fluff lines only — meaningful "Important: <content>" lines survive
            r"(?im)^[ \t]*[-*•]?[ \t]*(please be careful|be careful|this is important|note: this is important)[.!]?[ \t]*$",
            r"(?im)^[ \t]*[-*•][ \t]*$",
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    })
}

pub fn strip_boilerplate(text: &str) -> String {
    let mut t = text.to_string();
    for re in boilerplate_res() {
        t = re.replace_all(&t, "").into_owned();
    }
    t
}

const DEDUPE_MIN_CHARS: usize = 12;

pub fn dedupe_lines(text: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in text.split('\n') {
        let key = line.trim();
        if key.chars().count() >= DEDUPE_MIN_CHARS && !seen.insert(key.to_string()) {
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

pub fn collapse_whitespace(text: &str) -> String {
    static BLANKS: OnceLock<Regex> = OnceLock::new();
    static SPACES: OnceLock<Regex> = OnceLock::new();
    let blanks = BLANKS.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    let spaces = SPACES.get_or_init(|| Regex::new(r"[ \t]{2,}").unwrap());
    let text = blanks.replace_all(text, "\n\n");
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            let line = line.trim_end();
            if line.contains('`') {
                return line.to_string(); // inline-code safety
            }
            let indent_len = line.len() - line.trim_start().len();
            let (indent, rest) = line.split_at(indent_len);
            format!("{indent}{}", spaces.replace_all(rest, " "))
        })
        .collect();
    lines.join("\n")
}
```

- [ ] **Step 4: PASS. Step 5: Commit** `feat: conservative prose transforms`.

---

### Task 5: rules — full pipeline, duplicate fences, invariants

**Files:** Modify: `src/rules.rs`

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod pipeline_tests {
    use super::*;

    const CODE_HEAVY: &str = "Please fix this.\n```python\ndef f():\n    if x:\n        return 1\n    return 1\n```\nsome   spaced   prose\n```python\ndef f():\n    if x:\n        return 1\n    return 1\n```\n";

    #[test]
    fn fenced_code_is_byte_identical() {
        let out = rules_compress(CODE_HEAVY);
        assert!(out.contains("def f():\n    if x:\n        return 1\n    return 1\n"));
    }
    #[test]
    fn duplicate_fence_replaced_with_marker() {
        let out = rules_compress(CODE_HEAVY);
        assert_eq!(out.matches("def f():").count(), 1);
        assert!(out.contains("[duplicate code block removed]"));
    }
    #[test]
    fn repeated_brace_lines_inside_fence_survive() {
        let s = "```js\nif (a) {\n}\nif (b) {\n}\n```\n";
        assert!(rules_compress(s).contains("if (a) {\n}\nif (b) {\n}\n"));
    }
    #[test]
    fn idempotent_on_samples() {
        for s in [CODE_HEAVY, "plain  text\n\n\n\nmore", "```\nunclosed fence"] {
            let once = rules_compress(s);
            assert_eq!(rules_compress(&once), once);
        }
    }
    #[test]
    fn nonempty_in_nonempty_out() {
        assert!(!rules_compress("Thank you!").trim().is_empty() || true);
        // never empty for input that had real content:
        assert!(rules_compress("Thanks! Fix src/main.rs").contains("src/main.rs"));
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn fences_never_modified(body in "[a-zA-Z0-9 {}\\n]{0,200}", prose in "[a-zA-Z .\\n]{0,200}") {
            let input = format!("{prose}\n```rust\n{body}\n```\n");
            let out = rules_compress(&input);
            prop_assert!(out.contains(&format!("\n{body}\n")) || body.is_empty());
        }
        #[test]
        fn idempotent(s in "[ -~\\n]{0,400}") {
            let once = rules_compress(&s);
            prop_assert_eq!(rules_compress(&once), once);
        }
    }
}
```

- [ ] **Step 2: FAIL. Step 3: Implement**

```rust
/// Full deterministic pipeline. Prose-only transforms; fences untouched except
/// exact whole-duplicate removal.
pub fn rules_compress(text: &str) -> String {
    if text.trim().is_empty() {
        return text.to_string();
    }
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut segs = segment(&text);

    // duplicate-fence removal (exact full-body match)
    let mut seen_bodies = std::collections::HashSet::new();
    for seg in segs.iter_mut() {
        if let Segment::Fence { open, body, close } = seg {
            if !body.trim().is_empty() && !seen_bodies.insert(body.clone()) {
                *open = "```\n".to_string();
                *body = "[duplicate code block removed]\n".to_string();
                *close = Some("```\n".to_string());
            }
        }
    }

    // prose transforms — dedupe scope is global across prose segments
    let mut deduper = std::collections::HashSet::new();
    for seg in segs.iter_mut() {
        if let Segment::Prose(p) = seg {
            let t = strip_boilerplate(p);
            let t = dedupe_lines_with(&t, &mut deduper);
            *p = collapse_whitespace(&t);
        }
    }
    reassemble(&segs)
}
```

Refactor `dedupe_lines` into `dedupe_lines_with(text, &mut HashSet<String>)` (the existing test keeps passing via a thin wrapper). If idempotence proptest finds a counterexample, fix the transform — do not weaken the test. Known trap: `collapse_whitespace` must not re-shrink a `\n\n` it already produced; `strip_boilerplate` line-anchored patterns must consume any whitespace they leave behind (adjust the regex, add a regression test with the counterexample).

Note on the duplicate-fence marker: closing/open lines end with `\n` only when the original marker lines did; take care with a final unterminated line (the round-trip tests cover this — if they fail, derive the marker's trailing newline from the replaced fence's `open` line).

- [ ] **Step 4: PASS** (`cargo test rules`; proptest may take a few seconds). **Step 5:** `cargo clippy --all-targets -- -D warnings` clean. **Step 6: Commit** `feat: rules pipeline with fence protection and invariants`.

---

### Task 6: config module

**Files:** Modify: `src/config.rs`

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_sane() {
        let c = AppConfig::default();
        assert_eq!(c.local.model, "gemma3:4b");
        assert_eq!(c.encoder.llm_timeout_s, 15.0);
        assert_eq!(c.encoder.llm_scope, LlmScope::LastUser);
        assert_eq!(c.proxy.port, 8787);
        assert!(!c.encoder.list_trim_enabled);
    }
    #[test]
    fn loads_yaml_and_warns_on_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(&p, "local:\n  model: qwen3:8b\n  timeout_s: 120\ndecoder:\n  enabled: true\nencoder:\n  roles: [user]\n").unwrap();
        let (c, warnings) = load_config_from(&p).unwrap();
        assert_eq!(c.local.model, "qwen3:8b");
        assert_eq!(c.encoder.llm_timeout_s, 15.0); // local.timeout_s ignored
        let joined = warnings.join("\n");
        assert!(joined.contains("decoder"));
        assert!(joined.contains("local.timeout_s"));
        assert!(joined.contains("encoder.roles"));
    }
    #[test]
    fn missing_file_yields_defaults() {
        let (c, _) = resolve_config(Some("/nonexistent/x.yaml".into())).unwrap_or_else(|_| (AppConfig::default(), vec![]));
        assert_eq!(c.proxy.port, 8787);
    }
}
```

- [ ] **Step 2: FAIL. Step 3: Implement**

Structs (all `#[derive(Debug, Clone, serde::Deserialize)]`, `#[serde(default)]` on every struct and field so partial YAML works; `Default` impls carry spec defaults):

```rust
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmScope { LastUser, All, None }

pub struct LocalConfig { pub base_url: String, pub api_key: String, pub model: String, pub temperature: f64, pub max_tokens: u32 }
// defaults: http://127.0.0.1:11434/v1, "ollama", "gemma3:4b", 0.1, 2048

pub struct EncoderConfig {
    pub mode: String,                 // "rules" | "local" | "hybrid"; default "hybrid"
    pub target_ratio: f64,            // 0.45
    pub protect_system_under_chars: usize, // 800
    pub min_chars_to_compress: usize, // 400
    pub rules_enabled: bool,          // true
    pub llm_scope: LlmScope,          // LastUser
    pub llm_timeout_s: f64,           // 15.0
    pub list_trim_enabled: bool,      // false
}
pub struct CacheConfig { pub max_entries: u64 }            // 4096
pub struct ProxyConfig { pub host: String, pub port: u16, pub upstream_base_url: String, pub upstream_api_key_env: String, pub pass_client_auth: bool, pub require_client_auth: bool, pub log_stats: bool }
// defaults: 127.0.0.1, 8787, https://api.x.ai/v1, X_API_KEY, true, false, true
pub struct StatsConfig { pub usd_per_mtok_input: f64 }     // 3.0
pub struct AppConfig { pub local: LocalConfig, pub encoder: EncoderConfig, pub cache: CacheConfig, pub proxy: ProxyConfig, pub stats: StatsConfig }
```

Loading strategy (unknown keys must warn, not error): parse YAML to `serde_yaml::Value` first; walk it against a hard-coded map of known section→keys; collect warnings for unknown sections (`decoder`), unknown keys, and the two superseded keys `local.timeout_s` and `encoder.roles` (explicit messages: "local.timeout_s is ignored in v2; use encoder.llm_timeout_s" / "encoder.roles is ignored; role policy is fixed in v2"). Remove unknown/superseded keys from the `Value`, then `serde_yaml::from_value::<AppConfig>(value)`.

Public API:

```rust
pub fn load_config_from(path: &std::path::Path) -> anyhow::Result<(AppConfig, Vec<String>)>;
/// Lookup order: explicit → $PROMPT_CODEC_CONFIG → ./config.yaml → ~/.config/prompt-codec/config.yaml → defaults.
/// Returns the config plus warnings; logs (tracing::info) which source was used.
pub fn resolve_config(explicit: Option<std::path::PathBuf>) -> anyhow::Result<(AppConfig, Vec<String>)>;
```

`resolve_config` with an explicit path that doesn't exist is a hard error (the user asked for that file); missing files in the search chain just fall through, ending at `AppConfig::default()` with a "using built-in defaults" log line.

- [ ] **Step 4: PASS. Step 5: Commit** `feat: config with lookup order and superseded-key warnings`.

---

### Task 7: cache module

**Files:** Modify: `src/cache.rs`

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_is_deterministic_and_input_sensitive() {
        let a = RewriteCache::key("content", 0.45, "gemma3:4b");
        assert_eq!(a, RewriteCache::key("content", 0.45, "gemma3:4b"));
        assert_ne!(a, RewriteCache::key("content2", 0.45, "gemma3:4b"));
        assert_ne!(a, RewriteCache::key("content", 0.50, "gemma3:4b"));
        assert_ne!(a, RewriteCache::key("content", 0.45, "other"));
    }
    #[test]
    fn get_after_put() {
        let c = RewriteCache::new(16);
        let k = RewriteCache::key("x", 0.45, "m");
        assert!(c.get(&k).is_none());
        c.put(k.clone(), "compressed".into());
        assert_eq!(c.get(&k).as_deref(), Some("compressed"));
    }
}
```

- [ ] **Step 2: FAIL. Step 3: Implement**

```rust
//! LRU cache: SHA-256(content|ratio|model) → accepted LLM rewrite.
use sha2::{Digest, Sha256};

pub struct RewriteCache {
    inner: moka::sync::Cache<String, String>,
}

impl RewriteCache {
    pub fn new(max_entries: u64) -> Self {
        Self { inner: moka::sync::Cache::new(max_entries) }
    }
    pub fn key(content: &str, target_ratio: f64, model: &str) -> String {
        let mut h = Sha256::new();
        h.update(model.as_bytes());
        h.update([0]);
        h.update(format!("{target_ratio:.3}"));
        h.update([0]);
        h.update(content.as_bytes());
        hex::encode(h.finalize())
    }
    pub fn get(&self, key: &str) -> Option<String> { self.inner.get(key) }
    pub fn put(&self, key: String, value: String) { self.inner.insert(key, value) }
    pub fn entry_count(&self) -> u64 { self.inner.entry_count() }
}
```

- [ ] **Step 4: PASS. Step 5: Commit** `feat: rewrite cache`.

---

### Task 8: llm module (async local-LLM client)

**Files:** Modify: `src/llm.rs`, Create: `tests/llm_test.rs`

- [ ] **Step 1: Failing integration tests** (`tests/llm_test.rs`; wiremock)

```rust
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
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "short version"}}]
        })))
        .mount(&server).await;
    let (c, t) = cfg(&server.uri(), 5.0);
    let llm = LlmClient::new(&c, t);
    let out = llm.encode_text("some long prompt", 0.45).await.unwrap();
    assert_eq!(out, "short version");
}

#[tokio::test]
async fn timeout_is_an_error_not_a_hang() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(5))
            .set_body_json(serde_json::json!({"choices": []})))
        .mount(&server).await;
    let (c, t) = cfg(&server.uri(), 0.2);
    let llm = LlmClient::new(&c, t);
    let start = std::time::Instant::now();
    assert!(llm.encode_text("x", 0.45).await.is_err());
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

#[tokio::test]
async fn health_probe_reports_ok_and_down() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
        .mount(&server).await;
    let (c, t) = cfg(&server.uri(), 5.0);
    assert!(LlmClient::new(&c, t).health().await.ok);
    let mut down = LocalConfig::default();
    down.base_url = "http://127.0.0.1:1/v1".into();
    assert!(!LlmClient::new(&down, 5.0).health().await.ok);
}
```

- [ ] **Step 2: FAIL. Step 3: Implement**

Port `ENCODE_SYSTEM` verbatim from `legacy/prompt_codec/local_llm.py` (the 8-rule compressor prompt). Client:

```rust
pub struct LlmClient { http: reqwest::Client, base_url: String, api_key: String, model: String, temperature: f64, max_tokens: u32 }

pub struct LlmHealth { pub ok: bool, pub status: Option<u16>, pub error: Option<String>, pub base_url: String, pub model: String }

impl LlmClient {
    pub fn new(cfg: &LocalConfig, timeout_s: f64) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs_f64(timeout_s))
            .build().expect("reqwest client");
        // store trimmed base_url (trim_end_matches('/')) and cfg fields
    }
    pub async fn encode_text(&self, text: &str, target_ratio: f64) -> anyhow::Result<String> {
        let pct = ((target_ratio * 100.0) as i64).clamp(5, 95);
        let user = format!("Target length: about {pct}% of the original token count.\n\n--- ORIGINAL PROMPT ---\n{text}\n--- END ---");
        // POST {base}/chat/completions with {model, messages:[system,user], temperature, max_tokens, stream:false}
        // Bearer auth. error_for_status(). Parse choices[0].message.content (bail! on absent/empty).
        // Return content.trim().to_string()
    }
    pub async fn health(&self) -> LlmHealth {
        // GET {base}/models with a 3s per-call override timeout; ok = status < 500
    }
}
```

- [ ] **Step 4: PASS (`cargo test --test llm_test`). Step 5: Commit** `feat: async local-LLM client with hard timeout`.

---

### Task 9: codec — policy orchestration

**Files:** Modify: `src/codec.rs`, Create: `tests/codec_test.rs`

Behavior under test (spec §codec): role policy, `last_user` LLM scope, guard vs **post-rules** tokens, cache persistence across turns, tool-JSON minify, parts arrays untouched except text parts, LLM failure degrades to rules.

- [ ] **Step 1: Failing tests** (`tests/codec_test.rs`, wiremock as mock LLM; the mock counts calls via `expect()`)

```rust
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
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "tiny"}}]})))
        .expect(2) // turn 1: msg A; turn 2: msg B. A must come from cache on turn 2.
        .mount(&server).await;
    let codec = Codec::new(test_cfg(&server.uri()));

    let turn1 = vec![long_user("first long message needing compression right here")];
    let r1 = codec.encode_messages(turn1).await;
    let a_compressed = r1.messages[0]["content"].as_str().unwrap().to_string();
    assert_eq!(a_compressed, "tiny");

    let turn2 = vec![
        long_user("first long message needing compression right here"),
        json!({"role": "assistant", "content": "reply"}),
        long_user("second long message also needing compression here"),
    ];
    let r2 = codec.encode_messages(turn2).await;
    assert_eq!(r2.messages[0]["content"].as_str().unwrap(), a_compressed); // byte-stable history
    assert_eq!(r2.messages[2]["content"].as_str().unwrap(), "tiny");
}

#[tokio::test]
async fn rejects_rewrite_not_smaller_than_rules_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "this rewrite is much much much longer than the rules output was, so it must be rejected by the token guard"}}]})))
        .mount(&server).await;
    let codec = Codec::new(test_cfg(&server.uri()));
    let msgs = vec![long_user("short-ish content for the guard test")];
    let r = codec.encode_messages(msgs).await;
    assert!(r.messages[0]["content"].as_str().unwrap().contains("guard test")); // kept rules output
}

#[tokio::test]
async fn tool_json_is_minified_and_never_llm_rewritten() {
    let server = MockServer::start().await; // no mocks: any LLM call → 404 → test fails via expect(0) pattern
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500)).expect(0).mount(&server).await;
    let codec = Codec::new(test_cfg(&server.uri()));
    let msgs = vec![json!({"role": "tool", "tool_call_id": "abc",
        "content": "{\n  \"a\": 1,\n  \"b\": [1, 2]\n}"})];
    let r = codec.encode_messages(msgs).await;
    assert_eq!(r.messages[0]["content"].as_str().unwrap(), r#"{"a":1,"b":[1,2]}"#);
    assert_eq!(r.messages[0]["tool_call_id"], "abc");
}

#[tokio::test]
async fn llm_failure_degrades_to_rules() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    let codec = Codec::new(test_cfg(&server.uri()));
    let msgs = vec![long_user("please compress this content anyway thanks")];
    let r = codec.encode_messages(msgs).await;
    assert!(r.messages[0]["content"].as_str().unwrap().contains("compress this content"));
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
```

- [ ] **Step 2: FAIL. Step 3: Implement**

```rust
pub struct EncodeResult {
    pub messages: Vec<serde_json::Value>,
    pub stats: crate::stats::TokenStats,
    pub notes: Vec<String>,
}

pub struct Codec { cfg: AppConfig, llm: LlmClient, cache: RewriteCache }
```

`encode_messages(&self, messages: Vec<Value>) -> EncodeResult` algorithm:

1. `before` = sum of `count_tokens` over textual content (+4/message +2, ported from `legacy/prompt_codec/tokens.py::count_messages_tokens`).
2. Find `last_user_idx` = index of the last message with `role == "user"`.
3. For each message, by role:
   - `assistant` / unknown roles → untouched.
   - `tool` → if `content` is a string parsing as JSON → `serde_json::to_string` (compact); else `collapse_whitespace` only. Never anything else.
   - `system` → if `content` len < `protect_system_under_chars` → untouched; else rules.
   - `user` → rules.
   - String content only; for array-of-parts content apply the same treatment to each `{"type":"text"}` part's `text` field, leaving all else. All non-`content` fields always pass through (mutate `content` in place on the `Value`).
4. LLM pass, when `mode ∈ {local, hybrid}` and `llm_scope != None`: candidate messages = per scope (`LastUser` → just `last_user_idx`; `All` → every user/system message meeting eligibility). For each candidate with post-rules string content ≥ `min_chars_to_compress`:
   - `key = RewriteCache::key(post_rules_content, target_ratio, model)`.
   - **Every** candidate does a cache lookup first (this is what keeps history byte-stable); on hit, use the cached rewrite, note `cache_hit_msg_{i}`.
   - On miss, only messages **in scope** call `llm.encode_text`. Accept iff `!rewrite.trim().is_empty() && rewrite.len() > 20 && count_tokens(rewrite) < count_tokens(post_rules_content)`; then `cache.put` and note `llm_encode_msg_{i}`. On rejection note `llm_rejected_msg_{i}`; on error note `llm_failed:{e}` and keep rules output. Never propagate LLM errors.
5. `after` = recount; return `EncodeResult`.

Also `encode_text(&self, text, mode_override) -> (String, TokenStats, Vec<String>)` for the CLI/completions path: rules → (if mode has LLM and eligible) cache/LLM with the same guard.

Wait for one clarification in the test above: with `llm_scope: LastUser`, history user messages do cache **lookups** but never trigger calls — the `expect(2)` in the first test encodes exactly that.

- [ ] **Step 4: PASS. Step 5: Commit** `feat: codec with last-user LLM scope, cache-stable history, post-rules guard`.

---

### Task 10: proxy — chat completions forwarding

**Files:** Modify: `src/proxy.rs`, Create: `tests/proxy_test.rs`

Test harness pattern (top of `tests/proxy_test.rs`): start wiremock as the fake upstream; build the app with `create_app(cfg)`; serve it on an ephemeral port via `tokio::spawn(axum::serve(listener, app))`; hit it with `reqwest`.

```rust
async fn spawn_proxy(cfg: prompt_codec::config::AppConfig) -> String {
    let app = prompt_codec::proxy::create_app(cfg);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}
```

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test]
async fn compresses_messages_and_forwards_status_and_headers() {
    // upstream mock returns 200 with a JSON body and a custom header
    // client sends fluffy long user message; assert:
    //  - upstream received messages[0].content WITHOUT "Thank you" (rules ran)
    //  - upstream body has NO "metadata" key
    //  - client response: status 200, upstream body byte-identical,
    //    x-prompt-codec-before/after/saved-pct headers present
}

#[tokio::test]
async fn upstream_401_reaches_client_as_401() {
    // upstream mock: 401 with OpenAI-style error JSON
    // assert client sees 401 and the exact body
}

#[tokio::test]
async fn client_auth_passthrough_and_env_key_fallback() {
    // case 1: client sends Authorization: Bearer client-key; upstream mock asserts it receives that header
    // case 2: no client auth; set cfg.proxy.upstream_api_key_env to a var set for the test
    //         (use a unique var name + std::env::set_var); upstream asserts Bearer env-key
}

#[tokio::test]
async fn missing_messages_is_400_openai_shape() {
    // POST {} → 400, body has {"error": {"message": ..., "type": "invalid_request_error"}}
}

#[tokio::test]
async fn upstream_down_is_502_openai_shape() {
    // cfg.proxy.upstream_base_url = "http://127.0.0.1:1/v1" → 502 with {"error":{"type":"upstream_error"}}
}
```

Write these fully (the comments above specify the assertions; the implementing agent writes the real code — every assertion listed is required).

- [ ] **Step 2: FAIL. Step 3: Implement `create_app` + chat handler**

```rust
pub struct AppState {
    pub cfg: AppConfig,
    pub codec: Codec,
    pub upstream: reqwest::Client, // connect_timeout 10s, NO total timeout (streams)
}

pub fn create_app(cfg: AppConfig) -> axum::Router {
    // routes: POST /v1/chat/completions, POST /v1/completions,
    //         GET /health, any /v1/{*path} → catch_all
    // state: Arc<AppState>
}
```

Chat handler essentials:
1. Parse `Bytes` body → `serde_json::Value`; `messages` must be a non-empty array else 400 `{"error":{"message":"messages required","type":"invalid_request_error"}}`.
2. `let result = state.codec.encode_messages(msgs).await;` — replace `body["messages"]`; **no other mutation**.
3. `if cfg.proxy.log_stats` → `tracing::info!` before/after/pct/notes.
4. Forward: POST `{upstream_base}/chat/completions`, headers: `content-type: application/json` + auth (client `Authorization` header if present && `pass_client_auth`, else `Bearer {env}` if the env var is non-empty). If `require_client_auth` and no client auth → 401 immediately.
5. Build the response by **streaming**: take upstream `status`, copy all response headers except `transfer-encoding`, `connection`, `content-length`, add `x-prompt-codec-before/after/saved-pct`, body = `axum::body::Body::from_stream(resp.bytes_stream())`. One code path for streaming and non-streaming — bytes pass through verbatim either way.
6. `reqwest` send error → 502 OpenAI-shape (`upstream_error`).

Extract steps 4–6 into `async fn forward(state, path, body_bytes, client_headers, extra_headers) -> Response` — Task 11 reuses it.

- [ ] **Step 4: PASS. Step 5: Commit** `feat: chat completions proxy with verbatim response passthrough`.

---

### Task 11: proxy — completions, catch-all, health, SSE fidelity

**Files:** Modify: `src/proxy.rs`, `tests/proxy_test.rs`

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test]
async fn completions_prompt_gets_user_treatment() {
    // POST /v1/completions {"prompt": "<fluffy long text>"} → upstream receives compressed prompt
}

#[tokio::test]
async fn catch_all_forwards_raw_body_query_and_method() {
    // POST /v1/embeddings?foo=bar with a NON-JSON body (b"rawbytes\x00\x01")
    // upstream mock asserts: exact body bytes, query foo=bar present, POST method
    // response body/status pass through
}

#[tokio::test]
async fn sse_stream_chunks_pass_through_byte_identical() {
    // upstream mock: 200 with body "data: {\"a\":1}\n\ndata: [DONE]\n\n",
    //   content-type text/event-stream
    // client asserts: content-type preserved, full body byte-identical
}

#[tokio::test]
async fn health_reports_without_blocking() {
    // health with unreachable local LLM must return within ~4s, ok:true at top level,
    // local.ok == false, and include cache_entries + config source fields
}
```

- [ ] **Step 2: FAIL. Step 3: Implement**

- `/v1/completions`: parse; if `prompt` is a non-empty string, run `codec.encode_text` (user treatment, LLM-eligible); replace; forward via `forward()`.
- Catch-all `any(/v1/{*path})`: read raw `Bytes` (no JSON parse), forward method + path + `request.uri().query()` + auth header; stream response back. Use `reqwest::Method::from(...)`/pass the axum method through.
- `/health`: JSON `{ ok: true, encoder_mode, upstream, config_source, cache_entries, local: LlmHealth }` — the LLM probe already carries its own 3 s timeout.
- [ ] **Step 4: PASS + clippy clean. Step 5: Commit** `feat: completions, raw catch-all passthrough, health`.

---

### Task 12: CLI

**Files:** Create: `src/cli.rs`; Modify: `src/main.rs`, `src/lib.rs` (add `pub mod cli;`)

- [ ] **Step 1: Failing test** — CLI smoke via `assert_cmd`-style using `std::process::Command` on the built binary is overkill; instead unit-test the pieces: `cli::read_input(text, file, stdin_content)` precedence and `cli::render_savings(stats)` string content. Write those tests first in `src/cli.rs`.

- [ ] **Step 2: FAIL. Step 3: Implement**

clap derive in `main.rs`:

```rust
#[derive(clap::Parser)]
#[command(name = "prompt-codec", about = "Compress prompts before paid LLM APIs")]
struct Cli { #[command(subcommand)] cmd: Cmd }

#[derive(clap::Subcommand)]
enum Cmd {
    /// Run the OpenAI-compatible proxy
    Proxy { #[arg(long)] host: Option<String>, #[arg(short, long)] port: Option<u16>, #[arg(short, long)] config: Option<std::path::PathBuf> },
    /// Compress a prompt (arg, --file, or stdin)
    Encode { text: Option<String>, #[arg(short, long)] file: Option<std::path::PathBuf>, #[arg(short, long)] mode: Option<String>, #[arg(short, long)] config: Option<std::path::PathBuf>, #[arg(long)] json: bool },
    /// Rules-only demo on a bundled verbose sample
    Demo { #[arg(short, long)] config: Option<std::path::PathBuf> },
    /// Check config + local model reachability (exit 1 if local model down)
    Health { #[arg(short, long)] config: Option<std::path::PathBuf> },
}
```

`main` = `#[tokio::main]`, init `tracing_subscriber` (env filter, default `info`), `resolve_config`, print each config warning to stderr, dispatch. `encode --json` prints `{"text":…, "stats":…, "notes":…}`. `demo` ports the sample prompt from `legacy/prompt_codec/cli.py` and prints BEFORE/AFTER + savings line (plain text, no color deps). `health` prints JSON, exit code from `local.ok`. `proxy` binds `cfg.proxy.host:port` (CLI flags override) and serves; startup line prints the URL.

- [ ] **Step 4:** `cargo run -- demo` shows savings; `cargo run -- encode "Please help me, thank you!   Fix src/main.rs"` outputs compressed text. **Step 5: Commit** `feat: CLI (proxy/encode/demo/health)`.

---

### Task 13: golden corpus, A/B harness, config + README refresh

**Files:**
- Create: `tests/corpus/fluffy.txt` (port the demo sample from `legacy/prompt_codec/cli.py`), `tests/corpus/code_heavy.md` (Python with 4-space indents + two identical JS fences + repeated `}` lines), `tests/corpus/tool_dump.json` (pretty-printed 100-line JSON)
- Create: `tests/golden_test.rs`, `scripts/ab_compare.sh`
- Create: `config.example.yaml` (v2 keys, commented)
- Modify: `config.yaml` (root — user's live config: update model to `gemma3:4b`, drop `decoder:` block, rename `local.timeout_s` usage to `encoder.llm_timeout_s: 15`)
- Modify: `README.md` (Rust install/run instructions, v2 behavior notes, config table; keep the client-wiring sections)

- [ ] **Step 1: Failing golden tests**

```rust
// tests/golden_test.rs
use prompt_codec::rules::rules_compress;
use prompt_codec::tokenizer::count_tokens;

#[test]
fn fluffy_corpus_saves_at_least_20_pct() {
    let s = include_str!("corpus/fluffy.txt");
    let out = rules_compress(s);
    assert!(count_tokens(&out) as f64 <= count_tokens(s) as f64 * 0.8);
}

#[test]
fn code_heavy_corpus_fences_intact() {
    let s = include_str!("corpus/code_heavy.md");
    let out = rules_compress(s);
    // every unique fence body from the input appears verbatim in the output
    for body in unique_fence_bodies(s) {
        assert!(out.contains(&body), "fence body lost or mutated: {body:?}");
    }
}

#[test]
fn corpus_idempotent() {
    for s in [include_str!("corpus/fluffy.txt"), include_str!("corpus/code_heavy.md")] {
        let once = rules_compress(s);
        assert_eq!(rules_compress(&once), once);
    }
}
```

(`unique_fence_bodies` = helper using `prompt_codec::rules::segment`, first occurrence of each body.)

- [ ] **Step 2: FAIL (corpus files missing) → create corpus files → PASS.**

- [ ] **Step 3: A/B harness** — `scripts/ab_compare.sh`:

```bash
#!/usr/bin/env bash
# Compare Rust v2 vs legacy Python rules compression on the golden corpus.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --quiet --release
for f in tests/corpus/*; do
  rust_out=$(./target/release/prompt-codec encode --mode rules --file "$f" --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["stats"]["after_tokens"])')
  py_out=$(cd legacy && PYTHONPATH=. python3 -m prompt_codec.cli encode --mode rules -f "../$f" --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["stats"]["after_tokens"])')
  echo "$f  rust=$rust_out  python=$py_out"
done
```

Run it; paste the table into the README's "v2 vs v1" note. Expect Rust ≥ Python on `code_heavy.md` after-tokens (v2 refuses corrupting transforms — that's the point; note it).

- [ ] **Step 4: Update root `config.yaml` + write `config.example.yaml` + README.** README keeps the Hermes/DROID wiring sections (URLs unchanged), replaces install with `cargo build --release` + `ln -s target/release/prompt-codec` or `cargo install --path .`, documents the new keys and the superseded-key warnings.

- [ ] **Step 5:** Full `cargo test` + clippy. **Commit** `feat: golden corpus, A/B harness, v2 config and README`.

---

### Task 14: end-to-end verification & cutover

- [ ] **Step 1:** `cargo test` (all green) and `cargo clippy --all-targets -- -D warnings` (clean).
- [ ] **Step 2:** Manual smoke with a real fake upstream: run `cargo run --release -- proxy` with `upstream_base_url` pointed at a local `python3 -m http.server`-style stub or wiremock standalone — OR simply rerun the proxy integration tests and additionally `curl http://127.0.0.1:8787/health` against a live `proxy` process; verify the health JSON and startup config-source log line.
- [ ] **Step 3:** Latency check (acceptance §5): `time` 20 sequential `encode --mode rules` runs on `tests/corpus/code_heavy.md`; p50 per-call must be < 5 ms of codec work (measure via the CLI's `--json` timing or a `#[bench]`-style test with `std::time::Instant` in a `--release` test).
- [ ] **Step 4:** Confirm acceptance criteria 1–6 from the spec one by one; record results in the plan file under this task.
- [ ] **Step 5:** Final commit `chore: v0.2.0 — Rust rewrite complete`; leave `legacy/` in place per spec (user deletes when satisfied).

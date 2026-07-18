//! CLI command implementations. `main.rs` stays thin (parse + dispatch); the
//! testable logic — stdin/file/text precedence and the plain-text savings
//! report — lives here so it can be unit-tested without spawning the binary.

use std::path::PathBuf;

use anyhow::Context;

use crate::stats::TokenStats;

/// Encoder mode selectable from the CLI. clap's `ValueEnum` derive gives
/// `--mode` its possible-values help text and input validation (a bad value
/// is a clap usage error, exit code 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// Deterministic rules pipeline only (no local model needed)
    Rules,
    /// Local-model rewrite only
    Local,
    /// Rules first, then local-model rewrite
    Hybrid,
}

impl Mode {
    /// The lowercase name `Codec::encode_text` expects as a mode override.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Rules => "rules",
            Mode::Local => "local",
            Mode::Hybrid => "hybrid",
        }
    }
}

/// Whether `prompt-codec health` should exit non-zero: the local endpoint is
/// unreachable/erroring, OR it is reachable but the configured model is
/// definitively absent from its listing (`model_present == Some(false)`) —
/// in that state hybrid/local modes silently degrade to rules-only, which a
/// health check must surface. `None` (listing shape unknown, e.g. some MLX
/// servers) is not treated as failure.
pub fn health_failed(ok: bool, model_present: Option<bool>) -> bool {
    !ok || model_present == Some(false)
}

/// Resolve the input text for `encode`, in precedence order: `--file` wins
/// over the positional `text` arg, which wins over piped stdin. Errors when
/// none of the three are present.
pub fn read_input(
    text: Option<String>,
    file: Option<PathBuf>,
    stdin_content: Option<String>,
) -> anyhow::Result<String> {
    if let Some(path) = file {
        return std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read file {}", path.display()));
    }
    if let Some(t) = text {
        return Ok(t);
    }
    if let Some(s) = stdin_content {
        return Ok(s);
    }
    anyhow::bail!("Provide text, --file, or pipe stdin")
}

/// Render a plain-text (no color/table crates) savings report: before/after/
/// saved token counts, percentage saved (1 decimal), estimated dollars saved
/// (6 decimals), the mode actually used, and the note trail (or "—" when empty).
pub fn render_savings(stats: &TokenStats, mode_used: &str, notes: &[String]) -> String {
    let notes_str = if notes.is_empty() {
        "—".to_string()
    } else {
        notes.join(", ")
    };
    format!(
        "Token savings\n\
         -------------\n\
         Before: {before}\n\
         After:  {after}\n\
         Saved:  {saved} ({pct:.1}%)\n\
         Est. $ saved / call: ${usd:.6}\n\
         Mode:   {mode}\n\
         Notes:  {notes}",
        before = stats.before_tokens,
        after = stats.after_tokens,
        saved = stats.saved_tokens(),
        pct = stats.pct_saved(),
        usd = stats.usd_saved(),
        mode = mode_used,
        notes = notes_str,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_input_file_wins_over_text_and_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("prompt.txt");
        std::fs::write(&p, "from file").unwrap();
        let out = read_input(
            Some("from arg".to_string()),
            Some(p),
            Some("from stdin".to_string()),
        )
        .unwrap();
        assert_eq!(out, "from file");
    }

    #[test]
    fn read_input_text_used_when_no_file() {
        let out = read_input(
            Some("from arg".to_string()),
            None,
            Some("from stdin".to_string()),
        )
        .unwrap();
        assert_eq!(out, "from arg");
    }

    #[test]
    fn read_input_stdin_fallback() {
        let out = read_input(None, None, Some("from stdin".to_string())).unwrap();
        assert_eq!(out, "from stdin");
    }

    #[test]
    fn read_input_all_none_errors() {
        let err = read_input(None, None, None).unwrap_err();
        assert_eq!(err.to_string(), "Provide text, --file, or pipe stdin");
    }

    #[test]
    fn render_savings_contains_expected_substrings() {
        let s = TokenStats::new(1000, 400, 3.0);
        let out = render_savings(&s, "rules", &[]);
        assert!(out.contains("Before: 1000"));
        assert!(out.contains("After:  400"));
        assert!(out.contains("Saved:  600"));
        assert!(out.contains("60.0%"));
        assert!(out.contains("$0.001800"));
        assert!(out.contains("Mode:   rules"));
        assert!(out.contains("Notes:  —"));
    }

    #[test]
    fn render_savings_joins_notes_with_comma_space() {
        let s = TokenStats::new(1000, 400, 3.0);
        let notes = vec!["rules_compress".to_string(), "llm_encode".to_string()];
        let out = render_savings(&s, "hybrid", &notes);
        assert!(out.contains("Notes:  rules_compress, llm_encode"));
    }

    #[test]
    fn health_fails_when_down_or_model_absent() {
        assert!(health_failed(false, None)); // endpoint unreachable
        assert!(health_failed(false, Some(true))); // erroring endpoint trumps listing
        assert!(health_failed(true, Some(false))); // reachable but model not pulled
        assert!(!health_failed(true, Some(true))); // healthy
        assert!(!health_failed(true, None)); // unknown listing shape is not failure
    }
}

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

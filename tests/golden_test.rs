//! Golden-corpus tests: exercise `rules_compress` on realistic, larger inputs
//! (tests/corpus/*) rather than the small hand-crafted strings in
//! `src/rules.rs`'s unit tests. Guards two properties end-to-end:
//! - Real verbose prose still saves a meaningful fraction of tokens.
//! - Fenced code/JSON survives byte-for-byte (aside from exact-duplicate
//!   fence bodies, which are intentionally replaced with a marker).

use prompt_codec::rules::{rules_compress, segment, Segment};
use prompt_codec::tokenizer::count_tokens;

/// Unique (by content) non-empty fence bodies in `s`, in first-seen order.
/// A duplicated fence body is intentionally collapsed to one entry here: the
/// pipeline replaces the *second and later* occurrences with a marker, so
/// only the first occurrence's body is guaranteed to survive verbatim.
fn unique_fence_bodies(s: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    segment(s)
        .iter()
        .filter_map(|seg| match seg {
            Segment::Fence { body, .. } if !body.trim().is_empty() => Some(body.clone()),
            _ => None,
        })
        .filter(|b| seen.insert(b.clone()))
        .collect()
}

#[test]
fn fluffy_corpus_saves_at_least_20_pct() {
    let s = include_str!("corpus/fluffy.txt");
    let out = rules_compress(s);
    assert!(
        count_tokens(&out) as f64 <= count_tokens(s) as f64 * 0.8,
        "expected >=20% token savings on the fluffy demo sample"
    );
}

#[test]
fn code_heavy_corpus_fences_intact() {
    let s = include_str!("corpus/code_heavy.md");
    let out = rules_compress(s);
    for body in unique_fence_bodies(s) {
        assert!(out.contains(&body), "fence body lost or mutated: {body:?}");
    }
}

#[test]
fn tool_dump_fenced_json_untouched() {
    let s = include_str!("corpus/tool_dump.json");
    let out = rules_compress(s);
    for body in unique_fence_bodies(s) {
        assert!(out.contains(&body), "fenced JSON mutated: {body:?}");
    }
}

#[test]
fn corpus_idempotent() {
    for s in [
        include_str!("corpus/fluffy.txt"),
        include_str!("corpus/code_heavy.md"),
        include_str!("corpus/tool_dump.json"),
    ] {
        let once = rules_compress(s);
        assert_eq!(rules_compress(&once), once);
    }
}

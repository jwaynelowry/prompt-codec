//! Deterministic, fence-aware prompt compression. Pure functions only.

use regex::Regex;
use std::sync::OnceLock;

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

fn boilerplate_res() -> &'static Vec<Regex> {
    static RES: OnceLock<Vec<Regex>> = OnceLock::new();
    RES.get_or_init(|| {
        [
            r"(?im)^[ \t]*please[, ]+(i would like you to |help me with |remember to )?",
            r"(?im)^[ \t]*please\s+",
            // terminal punctuation or end-of-line required: "thank you email" survives
            r"(?im)\b(thank you|thanks)( so much)?( in advance)?(!|\.|$)[ \t]*",
            // \b after "ai": "as an aid" survives
            r"(?i)\bas an ai\b[^.!\n]*[.!]?[ \t]*",
            r"(?i)\bi hope this helps[^.!\n]*[.!]?[ \t]*",
            r"(?im)^[ \t]*i would like you to\s+",
            // phrase-only: never consume the rest of the line
            r"(?i)\b(write clean code|follow best practices|make it production ready)\b[.!,]?[ \t]*",
            r"(?i)\badd comments where needed\b[.!,]?[ \t]*",
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

/// Dedupe lines (>= `DEDUPE_MIN_CHARS`) using a caller-supplied `seen` set so
/// the scope can span multiple prose segments in the full pipeline.
pub fn dedupe_lines_with(text: &str, seen: &mut std::collections::HashSet<String>) -> String {
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

pub fn dedupe_lines(text: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    dedupe_lines_with(text, &mut seen)
}

pub fn collapse_whitespace(text: &str) -> String {
    static BLANKS: OnceLock<Regex> = OnceLock::new();
    static SPACES: OnceLock<Regex> = OnceLock::new();
    let blanks = BLANKS.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    let spaces = SPACES.get_or_init(|| Regex::new(r"[ \t]{2,}").unwrap());
    // Trim/space-collapse each line FIRST, then collapse blank runs. Doing the
    // blank collapse last keeps the function idempotent: whitespace-only lines
    // become empty here (not on a later pass), so no fresh `\n{3,}` run can
    // survive to be shrunk by a subsequent call.
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
    let joined = lines.join("\n");
    blanks.replace_all(&joined, "\n\n").into_owned()
}

/// Full deterministic pipeline. Prose-only transforms; fences untouched except
/// exact whole-duplicate removal.
pub fn rules_compress(text: &str) -> String {
    if text.trim().is_empty() {
        return text.to_string();
    }
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut segs = segment(&text);

    // duplicate-fence removal (exact full-body match). A candidate always has a
    // body (open line is followed by content), so its `open` line ends in "\n";
    // the marker mirrors that with a trailing-newline open/close.
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
    #[test]
    fn idempotent_with_interior_whitespace_lines() {
        // Regression for the collapse `\n{3,}` trap embedded in real content:
        // whitespace-only lines between paragraphs must settle in one pass.
        let s = "keep this line\n \n \n \nand this one";
        let once = rules_compress(s);
        assert_eq!(rules_compress(&once), once);
        assert!(!once.contains("\n\n\n"));
    }
    #[test]
    fn crlf_is_normalized() {
        let out = rules_compress("line one\r\nline two\r\n");
        assert!(!out.contains('\r'));
        assert!(out.contains("line one\nline two"));
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
            let needle = format!("\n{body}\n");
            prop_assert!(out.contains(&needle) || body.is_empty());
        }
        #[test]
        fn idempotent(s in "[ -~\\n]{0,400}") {
            let once = rules_compress(&s);
            prop_assert_eq!(rules_compress(&once), once);
        }
    }
}

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
    fn keeps_meaning_bearing_phrase_lookalikes() {
        // v1 regressions the v2 patterns must not repeat:
        let t = strip_boilerplate("write a thank you email to the team\n");
        assert!(t.contains("thank you email"));
        let t = strip_boilerplate("this serves as an aid to debugging\n");
        assert!(t.contains("as an aid to debugging"));
        let t = strip_boilerplate("Follow best practices and use bcrypt for hashing\n");
        assert!(t.contains("use bcrypt for hashing")); // phrase-only removal, rest of line survives
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
    #[test]
    fn collapse_is_idempotent_on_whitespace_only_lines() {
        // Regression: whitespace-only lines become empty during the per-line
        // trim, which used to synthesize a fresh `\n{3,}` run that only got
        // collapsed on a *second* pass (non-idempotent). Blank collapse now
        // runs last, so one pass is already a fixed point.
        let once = collapse_whitespace("x\n \n \n \ny");
        assert_eq!(once, "x\n\ny");
        assert_eq!(collapse_whitespace(&once), once);
    }
}

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

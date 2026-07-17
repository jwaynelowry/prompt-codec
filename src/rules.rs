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

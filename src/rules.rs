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

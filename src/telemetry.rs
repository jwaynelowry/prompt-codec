//! Cumulative savings telemetry (v0.3 Feature 3).
//!
//! Three pieces, all dependency-light and hot-path-safe:
//! - [`Totals`]: per-process atomic counters (requests, before/after tokens,
//!   upstream cached tokens, responses carrying cache info) plus a `since`
//!   RFC3339 timestamp. Serializes to/from a [`TotalsSnapshot`] for the cache
//!   `meta` table, so lifetime totals survive proxy restarts.
//! - [`TailBuffer`]: a fixed 16 KB ring that copies only the *last* bytes of a
//!   response body as chunks flow past — the proxy never buffers or parses a
//!   full body on the request path.
//! - [`extract_cached_tokens`]: a whitespace-tolerant regex scan of that tail
//!   for the upstream prompt-cache usage, matching BOTH the OpenAI
//!   (`cached_tokens`) and Anthropic/Z.ai (`cache_read_input_tokens`) shapes.
//!   Best-effort by design: absent usage simply records nothing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Fixed tail-capture window: the last 16 KB of a response body. Usage blocks
/// (non-streaming JSON, or the final SSE chunk under `include_usage`) live at
/// the end, so the tail is where the cached-token count shows up.
pub const TAIL_CAP: usize = 16 * 1024;

// --- Cumulative totals -------------------------------------------------------

/// Serializable snapshot of [`Totals`] — the exact shape persisted in the cache
/// `meta` table under `totals_json` and reloaded at proxy startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TotalsSnapshot {
    pub requests: u64,
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub upstream_cached_tokens: u64,
    pub responses_with_cache_info: u64,
    pub since: String,
}

/// Per-process cumulative savings counters. Atomics so the response-body
/// inspector (running on a stream-end poll) and the request handlers can update
/// concurrently without a lock. `saved_tokens` is *derived* on read
/// (`before - after`, saturating), never stored.
#[derive(Debug)]
pub struct Totals {
    requests: AtomicU64,
    before_tokens: AtomicU64,
    after_tokens: AtomicU64,
    upstream_cached_tokens: AtomicU64,
    responses_with_cache_info: AtomicU64,
    /// RFC3339 timestamp the totals row was first created. Persisted totals
    /// carry their original `since`; session-only totals use process start.
    since: String,
}

impl Totals {
    /// Fresh, all-zero totals stamped `since = now` (process start).
    pub fn fresh() -> Self {
        Self::from_snapshot(TotalsSnapshot {
            requests: 0,
            before_tokens: 0,
            after_tokens: 0,
            upstream_cached_tokens: 0,
            responses_with_cache_info: 0,
            since: now_rfc3339(),
        })
    }

    /// Rehydrate from a persisted snapshot (carrying its original `since`).
    pub fn from_snapshot(s: TotalsSnapshot) -> Self {
        Self {
            requests: AtomicU64::new(s.requests),
            before_tokens: AtomicU64::new(s.before_tokens),
            after_tokens: AtomicU64::new(s.after_tokens),
            upstream_cached_tokens: AtomicU64::new(s.upstream_cached_tokens),
            responses_with_cache_info: AtomicU64::new(s.responses_with_cache_info),
            since: s.since,
        }
    }

    /// Load totals from a persisted `totals_json` string. A parse failure warns
    /// once and degrades to [`Totals::fresh`] — a corrupt row never fails
    /// startup. `None` (no disk tier / no prior row) also yields fresh totals.
    pub fn load(persisted: Option<String>) -> Self {
        match persisted {
            Some(raw) => match serde_json::from_str::<TotalsSnapshot>(&raw) {
                Ok(snap) => Self::from_snapshot(snap),
                Err(e) => {
                    tracing::warn!(error = %e, "corrupt totals_json in cache meta; starting fresh");
                    Self::fresh()
                }
            },
            None => Self::fresh(),
        }
    }

    /// Read a consistent-enough snapshot for persistence and `/health`. Under
    /// concurrent updates the individual loads may straddle an increment; the
    /// worst case is off-by-one on a lifetime counter, which is acceptable.
    pub fn snapshot(&self) -> TotalsSnapshot {
        TotalsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            before_tokens: self.before_tokens.load(Ordering::Relaxed),
            after_tokens: self.after_tokens.load(Ordering::Relaxed),
            upstream_cached_tokens: self.upstream_cached_tokens.load(Ordering::Relaxed),
            responses_with_cache_info: self.responses_with_cache_info.load(Ordering::Relaxed),
            since: self.since.clone(),
        }
    }

    /// Count one compressed request. Returns the new cumulative request count so
    /// the caller can gate the "every 32 requests" flush.
    pub fn record_request(&self, before: u64, after: u64) -> u64 {
        self.before_tokens.fetch_add(before, Ordering::Relaxed);
        self.after_tokens.fetch_add(after, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Fold one upstream cached-token reading into the totals (called at
    /// response-stream end when the tail carried a usage block).
    pub fn record_cache_info(&self, cached_tokens: u64) {
        self.upstream_cached_tokens
            .fetch_add(cached_tokens, Ordering::Relaxed);
        self.responses_with_cache_info
            .fetch_add(1, Ordering::Relaxed);
    }
}

impl TotalsSnapshot {
    /// Derived lifetime tokens saved (`before - after`, clamped at zero — an
    /// expansion never reports negative savings).
    pub fn saved_tokens(&self) -> u64 {
        self.before_tokens.saturating_sub(self.after_tokens)
    }

    /// Derived estimated USD saved: `saved_tokens / 1e6 × usd_per_mtok_input`.
    pub fn est_usd_saved(&self, usd_per_mtok_input: f64) -> f64 {
        self.saved_tokens() as f64 / 1_000_000.0 * usd_per_mtok_input
    }
}

// --- Tail ring buffer --------------------------------------------------------

/// Fixed-capacity ring holding only the last [`TAIL_CAP`] bytes pushed. Cheap
/// to feed chunk-by-chunk from a streaming body; `as_bytes` reconstructs the
/// captured tail in order for a one-shot regex scan at stream end.
pub struct TailBuffer {
    buf: std::collections::VecDeque<u8>,
    cap: usize,
}

impl Default for TailBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl TailBuffer {
    pub fn new() -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(TAIL_CAP),
            cap: TAIL_CAP,
        }
    }

    /// Append `chunk`, keeping only the last `cap` bytes overall.
    pub fn push(&mut self, chunk: &[u8]) {
        // A single chunk larger than the window: keep only its own tail and skip
        // the pop loop entirely.
        if chunk.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&chunk[chunk.len() - self.cap..]);
            return;
        }
        self.buf.extend(chunk);
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
    }

    /// The captured tail as one contiguous slice (in push order).
    pub fn as_bytes(&mut self) -> &[u8] {
        self.buf.make_contiguous()
    }
}

// --- Cached-token extraction -------------------------------------------------

/// Scan a response-body tail for the upstream prompt-cache token count.
///
/// Matches BOTH usage shapes with whitespace tolerance:
/// - `"cached_tokens"\s*:\s*(\d+)` (OpenAI / xAI:
///   `usage.prompt_tokens_details.cached_tokens`)
/// - `"cache_read_input_tokens"\s*:\s*(\d+)` (Anthropic / Z.ai)
///
/// When both appear (or a value repeats across streamed deltas), the LAST match
/// wins — streaming finals arrive at the very end of the tail. Returns `None`
/// when neither shape is present (best-effort: absent usage records nothing).
pub fn extract_cached_tokens(tail: &[u8]) -> Option<u64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#""(?:cached_tokens|cache_read_input_tokens)"\s*:\s*(\d+)"#)
            .expect("cached-token regex is a valid literal")
    });
    let text = String::from_utf8_lossy(tail);
    re.captures_iter(&text)
        .last()
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<u64>().ok())
}

// --- RFC3339 timestamp (dependency-light) ------------------------------------

/// Current time as an RFC3339 UTC string (`YYYY-MM-DDThh:mm:ssZ`). Hand-rolled
/// so the crate never pulls in `chrono` just for one timestamp.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rfc3339_from_unix(secs)
}

/// Format unix seconds as an RFC3339 UTC string.
fn rfc3339_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Days since the unix epoch → `(year, month, day)`, via Howard Hinnant's
/// public-domain civil-from-days algorithm (correct for the proleptic
/// Gregorian calendar).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_buffer_keeps_only_last_16kb() {
        let mut tb = TailBuffer::new();
        // Push far more than the cap in many small chunks.
        for i in 0..(TAIL_CAP * 4) {
            let byte = [b'a' + (i % 26) as u8];
            tb.push(&byte);
        }
        // Then a distinctive marker that must be fully retained at the end.
        let marker = b"MARKER_AT_THE_VERY_END";
        tb.push(marker);
        let bytes = tb.as_bytes();
        assert_eq!(bytes.len(), TAIL_CAP, "ring never exceeds its cap");
        assert!(
            bytes.ends_with(marker),
            "the most recent bytes must survive"
        );
    }

    #[test]
    fn tail_buffer_oversized_single_chunk_keeps_its_own_tail() {
        let mut tb = TailBuffer::new();
        let mut big = vec![b'x'; TAIL_CAP + 500];
        big.extend_from_slice(b"TAILEND");
        tb.push(&big);
        let bytes = tb.as_bytes();
        assert_eq!(bytes.len(), TAIL_CAP);
        assert!(bytes.ends_with(b"TAILEND"));
    }

    #[test]
    fn extract_finds_cached_tokens_in_plain_json_tail() {
        let body = br#"{"id":"x","usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":40}}}"#;
        assert_eq!(extract_cached_tokens(body), Some(40));
    }

    #[test]
    fn extract_finds_cached_tokens_in_final_sse_chunk() {
        let sse = b"data: {\"choices\":[]}\n\ndata: {\"usage\":{\"prompt_tokens\":80,\"prompt_tokens_details\":{\"cached_tokens\": 12}}}\n\ndata: [DONE]\n\n";
        assert_eq!(extract_cached_tokens(sse), Some(12));
    }

    #[test]
    fn extract_matches_anthropic_cache_read_input_tokens() {
        // Anthropic / Z.ai shape (spec §4a).
        let body =
            br#"{"usage":{"input_tokens":200,"cache_read_input_tokens":150,"output_tokens":5}}"#;
        assert_eq!(extract_cached_tokens(body), Some(150));
    }

    #[test]
    fn extract_prefers_last_occurrence() {
        // Streaming deltas can repeat usage; the final value wins.
        let body = br#"{"cached_tokens": 10} ... {"cached_tokens":  99 }"#;
        assert_eq!(extract_cached_tokens(body), Some(99));
    }

    #[test]
    fn extract_returns_none_when_absent() {
        let body = br#"{"id":"x","choices":[{"message":{"content":"hi"}}]}"#;
        assert_eq!(extract_cached_tokens(body), None);
    }

    #[test]
    fn extract_finds_value_split_across_ring_boundary() {
        // Feed a body byte-by-byte through the ring so the number straddles the
        // 16 KB boundary, then extract from the reconstructed tail. The scan
        // must still find the value that ends the buffer.
        let filler = vec![b' '; TAIL_CAP]; // pushes the target toward the tail
        let payload = br#"trailing junk {"cached_tokens":  777 } end"#;
        let mut tb = TailBuffer::new();
        for b in filler.iter().chain(payload.iter()) {
            tb.push(&[*b]);
        }
        assert_eq!(extract_cached_tokens(tb.as_bytes()), Some(777));
    }

    #[test]
    fn totals_snapshot_roundtrips_through_json() {
        let t = Totals::from_snapshot(TotalsSnapshot {
            requests: 3,
            before_tokens: 1000,
            after_tokens: 400,
            upstream_cached_tokens: 80,
            responses_with_cache_info: 2,
            since: "2026-07-18T17:00:00Z".to_string(),
        });
        let json = serde_json::to_string(&t.snapshot()).unwrap();
        let back = Totals::load(Some(json));
        let s = back.snapshot();
        assert_eq!(s.requests, 3);
        assert_eq!(s.saved_tokens(), 600);
        assert_eq!(s.upstream_cached_tokens, 80);
        assert_eq!(s.responses_with_cache_info, 2);
        assert_eq!(s.since, "2026-07-18T17:00:00Z");
        assert!((s.est_usd_saved(3.0) - 0.0018).abs() < 1e-12);
    }

    #[test]
    fn totals_record_request_returns_running_count_and_sums() {
        let t = Totals::fresh();
        assert_eq!(t.record_request(100, 40), 1);
        assert_eq!(t.record_request(200, 90), 2);
        let s = t.snapshot();
        assert_eq!(s.requests, 2);
        assert_eq!(s.before_tokens, 300);
        assert_eq!(s.after_tokens, 130);
        assert_eq!(s.saved_tokens(), 170);
    }

    #[test]
    fn totals_saved_tokens_clamps_on_expansion() {
        let s = TotalsSnapshot {
            requests: 1,
            before_tokens: 100,
            after_tokens: 150,
            upstream_cached_tokens: 0,
            responses_with_cache_info: 0,
            since: "x".to_string(),
        };
        assert_eq!(s.saved_tokens(), 0);
    }

    #[test]
    fn load_corrupt_json_degrades_to_fresh() {
        let t = Totals::load(Some("this is not json".to_string()));
        assert_eq!(t.snapshot().requests, 0);
    }

    #[test]
    fn rfc3339_formats_known_timestamps() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        // The Unix "billennium": 1_000_000_000 = 2001-09-09T01:46:40Z.
        assert_eq!(rfc3339_from_unix(1_000_000_000), "2001-09-09T01:46:40Z");
    }
}

//! Before/after token stats and rough USD savings.
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

#[derive(Debug, Clone, Copy)]
pub struct TokenStats {
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub usd_per_mtok_input: f64,
}

impl TokenStats {
    pub fn new(before: usize, after: usize, usd_per_mtok: f64) -> Self {
        Self {
            before_tokens: before,
            after_tokens: after,
            usd_per_mtok_input: usd_per_mtok,
        }
    }
    pub fn saved_tokens(&self) -> usize {
        self.before_tokens.saturating_sub(self.after_tokens)
    }
    /// After/before compression ratio. Returns 1.0 when `before_tokens == 0`
    /// (nothing to compress counts as "unchanged", not a division error).
    pub fn ratio(&self) -> f64 {
        if self.before_tokens == 0 {
            1.0
        } else {
            self.after_tokens as f64 / self.before_tokens as f64
        }
    }
    pub fn pct_saved(&self) -> f64 {
        (1.0 - self.ratio()) * 100.0
    }
    /// Estimated USD saved. Clamps at zero by design (via `saved_tokens`'
    /// saturating subtraction) — an expansion never reports negative dollar
    /// savings, while `pct_saved` may honestly go negative.
    pub fn usd_saved(&self) -> f64 {
        self.saved_tokens() as f64 / 1_000_000.0 * self.usd_per_mtok_input
    }
}

/// Serializes the legacy `TokenStats.as_dict()` shape: the raw before/after
/// counts plus the derived savings fields (rounded), so downstream consumers
/// (proxy headers, `encode --json`) get the full picture.
impl Serialize for TokenStats {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut st = serializer.serialize_struct("TokenStats", 6)?;
        st.serialize_field("before_tokens", &self.before_tokens)?;
        st.serialize_field("after_tokens", &self.after_tokens)?;
        st.serialize_field("saved_tokens", &self.saved_tokens())?;
        st.serialize_field("ratio", &round_to(self.ratio(), 4))?;
        st.serialize_field("pct_saved", &round_to(self.pct_saved(), 2))?;
        st.serialize_field("usd_saved_est", &round_to(self.usd_saved(), 6))?;
        st.end()
    }
}

/// Round to `places` decimals. `pub(crate)` so the proxy's `/health` totals
/// use the same rounding (and 6-decimal USD precision) as `encode --json`.
pub(crate) fn round_to(x: f64, places: i32) -> f64 {
    let factor = 10f64.powi(places);
    (x * factor).round() / factor
}

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
    #[test]
    fn serializes_derived_fields() {
        let s = TokenStats::new(1000, 400, 3.0);
        let v = serde_json::to_value(s).unwrap();
        assert_eq!(v["before_tokens"], 1000);
        assert_eq!(v["after_tokens"], 400);
        assert_eq!(v["saved_tokens"], 600);
        assert_eq!(v["ratio"], 0.4);
        assert_eq!(v["pct_saved"], 60.0);
        assert_eq!(v["usd_saved_est"], 0.0018);
        // exactly the six keys of the legacy as_dict() shape
        assert_eq!(v.as_object().unwrap().len(), 6);
    }
}

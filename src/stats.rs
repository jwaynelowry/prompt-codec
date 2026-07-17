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
        Self {
            before_tokens: before,
            after_tokens: after,
            usd_per_mtok_input: usd_per_mtok,
        }
    }
    pub fn saved_tokens(&self) -> usize {
        self.before_tokens.saturating_sub(self.after_tokens)
    }
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
    pub fn usd_saved(&self) -> f64 {
        self.saved_tokens() as f64 / 1_000_000.0 * self.usd_per_mtok_input
    }
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
}

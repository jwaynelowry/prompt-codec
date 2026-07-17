//! LRU cache: SHA-256(content|ratio|model) -> accepted LLM rewrite.
use sha2::{Digest, Sha256};

pub struct RewriteCache {
    inner: moka::sync::Cache<String, String>,
}

impl RewriteCache {
    pub fn new(max_entries: u64) -> Self {
        Self {
            inner: moka::sync::Cache::new(max_entries),
        }
    }

    /// Deterministic key over the accept-cache identity: exact content, the
    /// target compression ratio (rounded to 3 decimals so float noise can't
    /// fragment the cache), and the model that produced/would produce the
    /// rewrite.
    pub fn key(content: &str, target_ratio: f64, model: &str) -> String {
        let mut h = Sha256::new();
        h.update(model.as_bytes());
        h.update([0]);
        h.update(format!("{target_ratio:.3}"));
        h.update([0]);
        h.update(content.as_bytes());
        hex::encode(h.finalize())
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.inner.get(key)
    }

    pub fn put(&self, key: String, value: String) {
        self.inner.insert(key, value);
    }

    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }
}

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

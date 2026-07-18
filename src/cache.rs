//! Two-tier rewrite cache: SHA-256(content|ratio|model) -> accepted LLM
//! rewrite.
//!
//! - **Hot tier** (`moka`, in-memory): the existing bounded LRU, unchanged.
//! - **Durable tier** (`rusqlite`, optional): survives process restarts and is
//!   shared with CLI one-shots, so compressed history stays byte-stable across
//!   restarts and no compression is ever paid for twice.
//!
//! The tiers are read-through/write-through: `get` checks memory, then disk
//! (promoting a disk hit back into memory); `put` writes both. Every disk error
//! (open, read, write, prune) logs a single `tracing::warn!` and degrades that
//! operation to memory-only — a broken disk cache never fails or slows a request
//! beyond the one failed call.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

/// Pragmas + schema for a fresh (or existing) DB. WAL is safe for the proxy
/// sharing the file with concurrent CLI one-shots; `busy_timeout` bounds a
/// contended lock. The `meta` table backs v0.3 Feature 3 (savings totals) and
/// carries a `schema_version` row for future migrations.
const SCHEMA_SQL: &str = "\
PRAGMA journal_mode=WAL;
PRAGMA busy_timeout=250;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS rewrites (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_rewrites_last_used ON rewrites(last_used);
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT);
INSERT OR IGNORE INTO meta(k, v) VALUES('schema_version', '1');
";

/// Process-wide dedup for open/create failures (spec: warn once per process).
static OPEN_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_open_once(err: &dyn std::fmt::Display) {
    if !OPEN_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(error = %err, "persistent rewrite cache unavailable; degrading to memory-only");
    }
}

/// Current unix time in seconds, saturating to 0 if the clock is before the
/// epoch (never panics).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Resolve the on-disk DB path: an explicit override, else
/// `dirs::cache_dir()/prompt-codec/rewrites.sqlite3`, else
/// `./prompt-codec-cache.sqlite3` when no platform cache dir resolves. Never
/// unwraps.
pub fn resolve_cache_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    match dirs::cache_dir() {
        Some(dir) => dir.join("prompt-codec").join("rewrites.sqlite3"),
        None => PathBuf::from("./prompt-codec-cache.sqlite3"),
    }
}

/// Inputs for building the durable tier. Built by `Codec::new` from
/// `CacheConfig` (only when `cache.persist` is true).
#[derive(Debug, Clone)]
pub struct DiskCacheConfig {
    pub path: PathBuf,
    pub max_disk_entries: u64,
}

/// The durable SQLite tier. `rusqlite::Connection` is not `Sync`, so it lives
/// behind `Arc<Mutex<..>>`; the `Arc`s make this cheap to `Clone` alongside the
/// already-`Clone` `RewriteCache`.
#[derive(Clone)]
struct DiskTier {
    conn: Arc<Mutex<Connection>>,
    max_disk_entries: u64,
    /// Cheap probabilistic prune gate: prune roughly every 1024 puts.
    put_counter: Arc<AtomicU64>,
    /// Once-per-instance warn dedup for read/write/prune failures.
    warned: Arc<AtomicBool>,
    path: PathBuf,
}

impl DiskTier {
    /// Open (or create) the DB and apply the schema. Any failure warns once and
    /// returns `None` — construction of the owning `RewriteCache` still
    /// succeeds, degraded to memory-only.
    fn open(cfg: DiskCacheConfig) -> Option<Self> {
        if let Some(parent) = cfg.path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    warn_open_once(&e);
                    return None;
                }
            }
        }
        let conn = match Connection::open(&cfg.path) {
            Ok(c) => c,
            Err(e) => {
                warn_open_once(&e);
                return None;
            }
        };
        if let Err(e) = conn.execute_batch(SCHEMA_SQL) {
            warn_open_once(&e);
            return None;
        }
        Some(Self {
            conn: Arc::new(Mutex::new(conn)),
            max_disk_entries: cfg.max_disk_entries,
            put_counter: Arc::new(AtomicU64::new(0)),
            warned: Arc::new(AtomicBool::new(false)),
            path: cfg.path,
        })
    }

    /// Lock the connection, recovering the guard even if a prior holder panicked
    /// (a poisoned mutex must never take down a request).
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn warn_once(&self, op: &str, err: &dyn std::fmt::Display) {
        if !self.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(operation = op, error = %err, "rewrite cache disk error; degrading to memory-only");
        }
    }

    /// Disk lookup. On a hit, best-effort touch `last_used` (a failed touch is
    /// ignored per spec). Any read error warns once and reports a miss.
    fn get(&self, key: &str) -> Option<String> {
        let conn = self.lock();
        let found: Option<String> = match conn
            .query_row(
                "SELECT value FROM rewrites WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()
        {
            Ok(v) => v,
            Err(e) => {
                self.warn_once("get", &e);
                return None;
            }
        };
        if found.is_some() {
            // Best-effort recency touch; a failure here is deliberately ignored.
            let _ = conn.execute(
                "UPDATE rewrites SET last_used = ?1 WHERE key = ?2",
                params![now_unix(), key],
            );
        }
        found
    }

    /// Write-through insert (INSERT OR REPLACE), then a probabilistic prune.
    fn put(&self, key: &str, value: &str) {
        let now = now_unix();
        {
            let conn = self.lock();
            if let Err(e) = conn.execute(
                "INSERT OR REPLACE INTO rewrites(key, value, created_at, last_used) \
                 VALUES(?1, ?2, ?3, ?4)",
                params![key, value, now, now],
            ) {
                self.warn_once("put", &e);
                return;
            }
        }
        // fetch_add returns the prior value; prune on 0, 1024, 2048, ... — the
        // first-put prune is a harmless no-op on an under-cap table.
        if self
            .put_counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(1024)
        {
            self.prune();
        }
    }

    /// Evict oldest-by-`last_used` rows beyond the cap. Uses the
    /// `key IN (SELECT ...)` form because bundled SQLite doesn't reliably
    /// compile `DELETE ... LIMIT`. The `rowid` tie-break makes eviction
    /// deterministic when several rows share a `last_used` second (older
    /// inserts go first).
    fn prune(&self) {
        let cap = self.max_disk_entries;
        let result: rusqlite::Result<()> = (|| {
            let conn = self.lock();
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM rewrites", [], |r| r.get(0))?;
            let count = count.max(0) as u64;
            if count > cap {
                let excess = (count - cap) as i64;
                conn.execute(
                    "DELETE FROM rewrites WHERE key IN (\
                        SELECT key FROM rewrites ORDER BY last_used ASC, rowid ASC LIMIT ?1)",
                    params![excess],
                )?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.warn_once("prune", &e);
        }
    }

    fn entry_count(&self) -> Option<u64> {
        let conn = self.lock();
        conn.query_row("SELECT COUNT(*) FROM rewrites", [], |r| r.get::<_, i64>(0))
            .ok()
            .map(|n| n.max(0) as u64)
    }

    fn meta_get(&self, k: &str) -> Option<String> {
        let conn = self.lock();
        conn.query_row("SELECT v FROM meta WHERE k = ?1", params![k], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .unwrap_or(None)
    }

    fn meta_set(&self, k: &str, v: &str) {
        let conn = self.lock();
        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES(?1, ?2)",
            params![k, v],
        ) {
            self.warn_once("meta_set", &e);
        }
    }
}

/// Two-tier cache of accepted LLM rewrites, keyed by [`RewriteCache::key`]. The
/// hot tier is an Arc-based `moka::sync::Cache`; the optional durable tier is a
/// shared SQLite connection. Clones are cheap handles onto the same underlying
/// stores — the codec and the proxy `AppState` share one cache this way.
#[derive(Clone)]
pub struct RewriteCache {
    inner: moka::sync::Cache<String, String>,
    disk: Option<DiskTier>,
}

impl RewriteCache {
    /// Memory-only cache bounded to `max_entries` (v0.2 behavior). Kept for
    /// callers/tests that never want a disk tier.
    pub fn new(max_entries: u64) -> Self {
        Self {
            inner: moka::sync::Cache::new(max_entries),
            disk: None,
        }
    }

    /// Cache with an optional durable tier. A `Some(cfg)` whose DB can't be
    /// opened degrades to memory-only (warned once) — construction never fails.
    pub fn new_with_disk(mem_entries: u64, disk: Option<DiskCacheConfig>) -> Self {
        Self {
            inner: moka::sync::Cache::new(mem_entries),
            disk: disk.and_then(DiskTier::open),
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

    /// Look up a rewrite: memory first, then disk (a disk hit is promoted back
    /// into memory). Marks the memory entry recently used.
    pub fn get(&self, key: &str) -> Option<String> {
        if let Some(v) = self.inner.get(key) {
            return Some(v);
        }
        let disk = self.disk.as_ref()?;
        let v = disk.get(key)?;
        self.inner.insert(key.to_string(), v.clone());
        Some(v)
    }

    /// Store an accepted rewrite under `key` in both tiers.
    pub fn put(&self, key: String, value: String) {
        if let Some(disk) = &self.disk {
            disk.put(&key, &value);
        }
        self.inner.insert(key, value);
    }

    /// Approximate number of live entries in the memory tier. Moka accounts for
    /// writes in batches (eventual consistency), so this can lag recent `put`s;
    /// call [`RewriteCache::sync`] first when an accurate reading matters.
    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }

    /// Row count of the durable tier, or `None` when persistence is off or the
    /// disk cache is broken. Reported by `/health` as `cache_disk_entries`.
    pub fn disk_entry_count(&self) -> Option<u64> {
        self.disk.as_ref()?.entry_count()
    }

    /// Resolved DB path when a durable tier is active, else `None`. Reported by
    /// `/health` as `cache_path`.
    pub fn disk_path(&self) -> Option<PathBuf> {
        self.disk.as_ref().map(|d| d.path.clone())
    }

    /// Read a `meta` row (v0.3 Feature 3 totals). `None` without a disk tier.
    pub fn meta_get(&self, k: &str) -> Option<String> {
        self.disk.as_ref()?.meta_get(k)
    }

    /// Write a `meta` row (v0.3 Feature 3 totals). A silent no-op without a disk
    /// tier.
    pub fn meta_set(&self, k: &str, v: &str) {
        if let Some(disk) = &self.disk {
            disk.meta_set(k, v);
        }
    }

    /// Test hook: force a disk prune regardless of the probabilistic gate, so
    /// prune tests don't need ~1024 filler puts.
    #[cfg(test)]
    pub(crate) fn prune_now(&self) {
        if let Some(disk) = &self.disk {
            disk.prune();
        }
    }

    /// Flush moka's pending internal tasks (batched write accounting,
    /// evictions) so that [`RewriteCache::entry_count`] reflects all prior
    /// writes. The `/health` endpoint calls this before reporting
    /// `cache_entries`.
    pub fn sync(&self) {
        self.inner.run_pending_tasks();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn disk_cfg(dir: &Path, max: u64) -> DiskCacheConfig {
        DiskCacheConfig {
            path: dir.join("rewrites.sqlite3"),
            max_disk_entries: max,
        }
    }

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

    #[test]
    fn clones_share_the_underlying_store() {
        let a = RewriteCache::new(16);
        let b = a.clone();
        a.put("k".into(), "v".into());
        assert_eq!(b.get("k").as_deref(), Some("v"));
    }

    #[test]
    fn entry_count_is_accurate_after_sync() {
        let c = RewriteCache::new(16);
        c.put("k1".into(), "v1".into());
        c.put("k2".into(), "v2".into());
        c.sync();
        assert_eq!(c.entry_count(), 2);
    }

    #[test]
    fn disk_roundtrip_survives_new_instance() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = disk_cfg(dir.path(), 100_000);
        let k = RewriteCache::key("x", 0.45, "m");
        {
            let a = RewriteCache::new_with_disk(16, Some(cfg.clone()));
            a.put(k.clone(), "compressed value here".into());
        } // drop A: memory tier gone, only the disk row remains
        let b = RewriteCache::new_with_disk(16, Some(cfg));
        assert_eq!(b.get(&k).as_deref(), Some("compressed value here"));
        assert_eq!(b.disk_entry_count(), Some(1));
    }

    #[test]
    fn disk_miss_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let c = RewriteCache::new_with_disk(16, Some(disk_cfg(dir.path(), 100_000)));
        assert!(c.get("no-such-key").is_none());
        assert_eq!(c.disk_entry_count(), Some(0));
    }

    #[test]
    fn prune_caps_disk_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = disk_cfg(dir.path(), 8);
        {
            // Cap 8, insert 20 distinct rows, then force a prune via the hook.
            let c = RewriteCache::new_with_disk(64, Some(cfg.clone()));
            for i in 0..20 {
                c.put(
                    RewriteCache::key(&format!("k{i}"), 0.45, "m"),
                    format!("value number {i} with padding"),
                );
            }
            c.prune_now();
            let count = c.disk_entry_count().unwrap();
            assert!(count <= 8, "expected <= 8 rows after prune, got {count}");
        } // drop: the roomy (64) memory tier would otherwise mask disk eviction.
          // Re-open with an empty memory tier so `get` reflects the DISK rows.
        let fresh = RewriteCache::new_with_disk(64, Some(cfg));
        assert!(
            fresh.get(&RewriteCache::key("k19", 0.45, "m")).is_some(),
            "the newest insert must survive prune"
        );
        assert!(
            fresh.get(&RewriteCache::key("k0", 0.45, "m")).is_none(),
            "the oldest insert must be evicted (last_used ASC, rowid ASC)"
        );
    }

    #[test]
    fn corrupt_db_degrades_to_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.sqlite3");
        std::fs::write(&path, b"this is not a sqlite database, just garbage bytes").unwrap();
        let cfg = DiskCacheConfig {
            path,
            max_disk_entries: 100,
        };
        // Construction must SUCCEED (degrade), never panic.
        let c = RewriteCache::new_with_disk(16, Some(cfg));
        assert!(
            c.disk_entry_count().is_none(),
            "a corrupt DB has no usable disk tier"
        );
        // put/get still work via the memory tier.
        let k = RewriteCache::key("x", 0.45, "m");
        c.put(k.clone(), "in-memory value only".into());
        assert_eq!(c.get(&k).as_deref(), Some("in-memory value only"));
    }

    #[test]
    fn persist_false_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // No disk config: memory-only, and the target dir stays empty.
        let c = RewriteCache::new_with_disk(16, None);
        let k = RewriteCache::key("x", 0.45, "m");
        c.put(k.clone(), "value".into());
        assert_eq!(c.get(&k).as_deref(), Some("value"));
        assert!(c.disk_entry_count().is_none());
        assert!(c.disk_path().is_none());
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(
            entries.is_empty(),
            "nothing should be written when persistence is off"
        );
    }

    #[test]
    fn meta_get_set_roundtrip_and_none_without_disk() {
        let dir = tempfile::tempdir().unwrap();
        let c = RewriteCache::new_with_disk(16, Some(disk_cfg(dir.path(), 100)));
        assert!(c.meta_get("totals_json").is_none());
        c.meta_set("totals_json", r#"{"requests":2}"#);
        assert_eq!(
            c.meta_get("totals_json").as_deref(),
            Some(r#"{"requests":2}"#)
        );
        // Without a disk tier, meta ops are silent no-ops.
        let mem = RewriteCache::new(16);
        mem.meta_set("k", "v");
        assert!(mem.meta_get("k").is_none());
    }
}

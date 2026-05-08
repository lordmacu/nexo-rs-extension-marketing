//! Notification dedup cache (M15.53 / F9).
//!
//! NATS at-least-once delivery means a transient broker
//! disconnect can re-deliver the same `plugin.inbound.email.*`
//! event after a reconnect. The marketing extension's broker
//! hop is idempotent at the **lead** layer (the
//! `find_by_thread → Some(lead)` short-circuit handles the
//! replay) but the **notification publish** runs on every
//! event regardless of the lead branch — so a duplicate
//! inbound delivers a duplicate `EmailNotification` to the
//! operator's WhatsApp / email.
//!
//! This module provides a simple in-memory time-bucket dedup
//! cache: dedupe key → wall-clock instant of last publish.
//! Future publishes that hit a fresh entry are silently
//! suppressed.
//!
//! ## Scope + threat model
//!
//! - **In-process replays** (NATS reconnect, broker hop
//!   crash before reply, etc.): caught.
//! - **Cross-restart replays** (extension crash + NATS
//!   redelivers from durable queue on next boot): NOT caught
//!   — the cache is lost on process exit.
//!
//! The latter can be addressed later by swapping the inner
//! `HashMap` for a `sled`-backed store; the public surface is
//! kept narrow on purpose so that swap is a 1-file change.
//! Cross-restart dedup is rare in practice for marketing
//! notifications (operator restarts the extension manually,
//! NATS doesn't retain plugin.inbound.* events across
//! consumer restarts in our config) so the in-memory cache
//! covers ~95% of the threat with zero new deps.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default dedup TTL — events older than this get evicted on
/// the next `is_duplicate` call. 1 hour catches the typical
/// at-least-once redelivery window for NATS while keeping the
/// cache small (typical marketing tenant: <1000 entries).
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// Identifier for a notification publish call. Two publishes
/// with the same key within the TTL window are deduped — only
/// the first lands on the broker.
///
/// Caller convention: include the lead id + the kind + an
/// `at_ms` bucket (typically the wall-clock minute) so a
/// genuinely-different at-the-same-second event from the same
/// lead doesn't get suppressed. The marketing extension uses
/// `(tenant_id, lead_id, kind, at_ms / 60_000)` per-publish.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupKey(String);

impl DedupKey {
    /// Build from the canonical 4-tuple. `at_ms` is rounded
    /// down to the minute so two events ~30 s apart on the
    /// same lead+kind dedupe; events ~5 min apart don't.
    pub fn new(tenant_id: &str, lead_id: &str, kind: &str, at_ms: i64) -> Self {
        let bucket = at_ms / 60_000;
        Self(format!("{tenant_id}:{lead_id}:{kind}:{bucket}"))
    }

    /// Internal accessor for the canonical string form.
    /// Public so the caller can log it for debugging.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Lock-protected in-memory dedup cache. `Mutex` (not
/// `RwLock`) because every read upgrades to a write to record
/// the freshly-deduped timestamp; `RwLock` would just
/// pessimise.
///
/// Eviction runs lazily inside `is_duplicate` so we don't
/// need a background sweeper task — the worst-case is that a
/// cold cache holds expired entries until the next call. For
/// marketing's volume (<1 publish/sec/tenant), this is a
/// non-issue.
#[derive(Debug)]
pub struct DedupCache {
    inner: Mutex<HashMap<DedupKey, Instant>>,
    ttl: Duration,
}

impl Default for DedupCache {
    fn default() -> Self {
        Self::with_ttl(DEFAULT_TTL)
    }
}

impl DedupCache {
    /// Build a cache with the framework default TTL (1 hour).
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a cache with a caller-supplied TTL. Tests use a
    /// short TTL to exercise the eviction path without
    /// needing `tokio::time::pause`.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Returns `true` if the key is a duplicate of a recent
    /// publish (within TTL); records the call regardless so
    /// subsequent calls within TTL are also deduped.
    ///
    /// Concurrent calls are serialised on the internal mutex.
    /// The lock guard scope is tight (1 HashMap op + 1
    /// Instant::now); contention is negligible for marketing
    /// volumes.
    pub fn is_duplicate(&self, key: &DedupKey) -> bool {
        let now = Instant::now();
        let mut map = self.inner.lock().expect("notification dedup poisoned");
        // Lazy eviction — drop every entry whose timestamp
        // is older than `ttl`. O(n) sweep but n is bounded
        // (per-tenant per-hour publish count).
        map.retain(|_, t| now.duration_since(*t) < self.ttl);
        match map.get(key) {
            Some(_) => true,
            None => {
                map.insert(key.clone(), now);
                false
            }
        }
    }

    /// Number of live entries — used by tests + the optional
    /// `/healthz` debug surface.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// `true` when no live entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(lead: &str) -> DedupKey {
        DedupKey::new("acme", lead, "lead_created", 1_700_000_000_000)
    }

    #[test]
    fn first_call_returns_false_records_entry() {
        let cache = DedupCache::new();
        assert!(!cache.is_duplicate(&k("l-1")));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn second_call_with_same_key_returns_true() {
        let cache = DedupCache::new();
        assert!(!cache.is_duplicate(&k("l-1")));
        assert!(cache.is_duplicate(&k("l-1")));
        // Cache size doesn't grow — same key.
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn distinct_lead_ids_are_independent() {
        let cache = DedupCache::new();
        assert!(!cache.is_duplicate(&k("l-1")));
        assert!(!cache.is_duplicate(&k("l-2")));
        assert!(cache.is_duplicate(&k("l-1")));
        assert!(cache.is_duplicate(&k("l-2")));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn distinct_kinds_are_independent() {
        let cache = DedupCache::new();
        let k_created = DedupKey::new("acme", "l-1", "lead_created", 0);
        let k_replied = DedupKey::new("acme", "l-1", "lead_replied", 0);
        assert!(!cache.is_duplicate(&k_created));
        assert!(!cache.is_duplicate(&k_replied));
        // Re-fire each — both deduped.
        assert!(cache.is_duplicate(&k_created));
        assert!(cache.is_duplicate(&k_replied));
    }

    #[test]
    fn distinct_minute_buckets_are_independent() {
        let cache = DedupCache::new();
        // Two events on the same lead+kind, 5 minutes apart.
        // Bucket boundary at minute → distinct keys.
        let k_a = DedupKey::new("acme", "l-1", "lead_created", 0);
        let k_b = DedupKey::new("acme", "l-1", "lead_created", 5 * 60_000);
        assert!(!cache.is_duplicate(&k_a));
        assert!(!cache.is_duplicate(&k_b));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn cross_tenant_keys_are_independent() {
        let cache = DedupCache::new();
        let acme = DedupKey::new("acme", "l-1", "lead_created", 0);
        let globex = DedupKey::new("globex", "l-1", "lead_created", 0);
        assert!(!cache.is_duplicate(&acme));
        assert!(!cache.is_duplicate(&globex));
        assert!(cache.is_duplicate(&acme));
        assert!(cache.is_duplicate(&globex));
    }

    #[test]
    fn ttl_expired_entries_are_evicted_on_next_call() {
        let cache = DedupCache::with_ttl(Duration::from_millis(50));
        assert!(!cache.is_duplicate(&k("l-1")));
        assert_eq!(cache.len(), 1);
        std::thread::sleep(Duration::from_millis(60));
        // Sleep window > TTL → first call's entry expired.
        // Next call on the same key returns false (treated
        // as fresh) AND the lazy sweep drops the stale row
        // before insert.
        assert!(!cache.is_duplicate(&k("l-1")));
        assert_eq!(cache.len(), 1, "stale entry should be replaced not stacked");
    }

    #[test]
    fn dedup_key_minute_bucket_collapses_30s_apart_events() {
        // Same minute bucket → identical key → second event
        // deduped against the first.
        let a = DedupKey::new("acme", "l-1", "lead_created", 0);
        let b = DedupKey::new("acme", "l-1", "lead_created", 30 * 1000);
        assert_eq!(a, b);
    }
}

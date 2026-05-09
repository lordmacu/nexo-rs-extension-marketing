//! In-memory per-signature lock to coalesce concurrent draft
//! generations (option **J** in the cost-control plan).
//!
//! Closes the race that the SQLite `find_pending_draft_by_signature`
//! pre-LLM lookup leaves open: two concurrent requests with the
//! same signature both miss the DB at step 1, both spend tokens
//! at step 2, both persist at step 3 (each with their own
//! `message_id`), leaving two pending drafts with identical
//! signatures in the table.
//!
//! After this module: the second request blocks on the
//! per-signature mutex until the first finishes its LLM call +
//! persist. When it acquires the lock it re-queries the DB and
//! finds the freshly-persisted draft → returns it without
//! spending any tokens of its own.
//!
//! Quality semantics are identical to **B** alone — same input
//! ⇒ same output, just without the wasted parallel call.
//!
//! The lock granularity is `(tenant_id, signature)` (caller
//! prefixes the key). Map grows monotonically over the process
//! lifetime; entries are tiny (Arc + Mutex<()>) so we accept
//! this until volume forces us to add a periodic GC pass.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// Shared lock map. Cheap to clone — internals are already
/// behind an `Arc`.
#[derive(Clone, Default)]
pub struct DraftLockMap {
    map: Arc<StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
}

impl DraftLockMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire (or wait for) the lock matching `key`. Returns
    /// the owned guard; drop it to release. The async wait
    /// happens on the inner `tokio::sync::Mutex`; the outer
    /// std-mutex is held only for the get-or-insert and is
    /// released before the await.
    pub async fn acquire(&self, key: &str) -> OwnedMutexGuard<()> {
        let inner = {
            let mut g = self
                .map
                .lock()
                .expect("DraftLockMap poisoned — bug elsewhere");
            g.entry(key.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        inner.lock_owned().await
    }

    /// Diagnostic only — number of distinct keys with at least
    /// one in-flight or recently-acquired entry.
    #[cfg(test)]
    pub fn key_count(&self) -> usize {
        self.map.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Concurrent acquire on the SAME key serializes — only one
    /// guard is held at a time. We assert by tracking the max
    /// concurrent counter as both tasks race.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_acquire_same_key_serializes() {
        let map = DraftLockMap::new();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mk_task = || {
            let map = map.clone();
            let in_flight = in_flight.clone();
            let max_seen = max_seen.clone();
            tokio::spawn(async move {
                let _g = map.acquire("sig-x").await;
                let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(cur, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            })
        };
        let a = mk_task();
        let b = mk_task();
        let _ = tokio::join!(a, b);
        assert_eq!(max_seen.load(Ordering::SeqCst), 1, "two tasks held the same-key guard at once");
    }

    /// Different keys do NOT serialize — both tasks can run in
    /// parallel. Counter-test for the previous one.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_acquire_different_keys_overlap() {
        let map = DraftLockMap::new();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let task = |key: &'static str| {
            let map = map.clone();
            let in_flight = in_flight.clone();
            let max_seen = max_seen.clone();
            tokio::spawn(async move {
                let _g = map.acquire(key).await;
                let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(cur, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            })
        };
        let a = task("sig-a");
        let b = task("sig-b");
        let _ = tokio::join!(a, b);
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            2,
            "different keys must be able to run concurrently",
        );
    }

    #[tokio::test]
    async fn key_count_grows_with_distinct_keys() {
        let map = DraftLockMap::new();
        assert_eq!(map.key_count(), 0);
        let _g1 = map.acquire("a").await;
        let _g2 = map.acquire("b").await;
        assert_eq!(map.key_count(), 2);
    }
}

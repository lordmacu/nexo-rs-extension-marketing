//! Lightweight counters tracking broker-hop in-flight depth +
//! lifetime totals (audit fix #8).
//!
//! NATS itself doesn't expose queue depth from the client side
//! — the broker holds messages in memory until our subscriber
//! polls them via the SDK's `BrokerEventHandler`. When the
//! handler runs slowly (LLM call, scrape, slow disk), the
//! incoming queue grows and we silently fall behind. This
//! module gives the operator a number they can alarm on:
//!
//! - `in_flight`: how many handler invocations are currently
//!   running concurrently. A steady non-zero value while
//!   nothing is arriving means a hung handler.
//! - `max_in_flight_seen`: peak concurrency since process
//!   start. High values relative to the worker pool size hint
//!   at backpressure.
//! - `processed_total`: lifetime success counter so the
//!   operator can compute "processed/sec" by sampling.
//!
//! The metrics surface via `GET /healthz` (open + cheap) +
//! the existing telemetry endpoint, so dashboards already
//! polling for liveness pick up backlog signals automatically.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Atomic snapshot of the broker hop's in-flight + lifetime
/// counters. Cheap to clone (Arc-shared internals); the broker
/// hop closure clones once and increments / decrements per
/// event, the healthz handler clones once and reads.
#[derive(Clone, Default)]
pub struct BrokerMetrics {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    in_flight: AtomicU64,
    max_in_flight_seen: AtomicU64,
    processed_total: AtomicU64,
}

impl BrokerMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a broker event as entering the handler. Returns a
    /// guard whose `Drop` decrements `in_flight` so the caller
    /// can't forget. Updates `max_in_flight_seen` atomically.
    pub fn enter(&self) -> InFlightGuard {
        let entered = self.inner.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        // Update the high-water mark with a CAS loop so two
        // simultaneous enters can't both clobber it with their
        // own (lower) view of the maximum.
        let mut max = self.inner.max_in_flight_seen.load(Ordering::SeqCst);
        while entered > max {
            match self.inner.max_in_flight_seen.compare_exchange_weak(
                max,
                entered,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => max = actual,
            }
        }
        InFlightGuard {
            metrics: self.clone(),
        }
    }

    /// Mark a successfully-processed event. Increments
    /// `processed_total` independent of whether `enter()` was
    /// called — the broker hop calls this from the success
    /// branches.
    pub fn record_processed(&self) {
        self.inner.processed_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot for the healthz / telemetry handlers.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            in_flight: self.inner.in_flight.load(Ordering::SeqCst),
            max_in_flight_seen: self.inner.max_in_flight_seen.load(Ordering::SeqCst),
            processed_total: self.inner.processed_total.load(Ordering::Relaxed),
        }
    }
}

/// Decrement-on-drop guard returned by [`BrokerMetrics::enter`].
/// Capture into a `let _g = ...;` binding for the duration of
/// the handler body.
pub struct InFlightGuard {
    metrics: BrokerMetrics,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.metrics.inner.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct MetricsSnapshot {
    pub in_flight: u64,
    pub max_in_flight_seen: u64,
    pub processed_total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_increments_in_flight_and_drops_decrement() {
        let m = BrokerMetrics::new();
        assert_eq!(m.snapshot().in_flight, 0);
        let g = m.enter();
        assert_eq!(m.snapshot().in_flight, 1);
        drop(g);
        assert_eq!(m.snapshot().in_flight, 0);
    }

    #[test]
    fn max_in_flight_tracks_high_water() {
        let m = BrokerMetrics::new();
        let g1 = m.enter();
        let g2 = m.enter();
        let g3 = m.enter();
        assert_eq!(m.snapshot().max_in_flight_seen, 3);
        drop(g3);
        drop(g2);
        // High-water mark stays at 3 even after some drop.
        assert_eq!(m.snapshot().max_in_flight_seen, 3);
        assert_eq!(m.snapshot().in_flight, 1);
        drop(g1);
    }

    #[test]
    fn processed_total_is_independent_of_in_flight() {
        let m = BrokerMetrics::new();
        m.record_processed();
        m.record_processed();
        m.record_processed();
        assert_eq!(m.snapshot().processed_total, 3);
        assert_eq!(m.snapshot().in_flight, 0);
    }
}

//! Notification dedup cache.
//!
//! Lifted to the SDK in F29 (Phase 83.18 / SDK 0.1.11) — this
//! module now re-exports `nexo_microapp_sdk::dedup` so existing
//! call sites keep compiling without churn. New consumers
//! should reach for the SDK path directly.
//!
//! Marketing-specific by design: nothing — pure pass-through.
//! The `dedup-sled` feature on this crate forwards to the
//! SDK's identically-named feature.

pub use nexo_microapp_sdk::dedup::{DedupCache, DedupKey, DEFAULT_TTL};

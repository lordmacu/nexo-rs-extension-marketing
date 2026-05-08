//! Email subject normalization + synthetic thread-id helpers.
//!
//! Lifted to the SDK in F29 (Phase 83.18 / SDK 0.1.11) — this
//! module now re-exports `nexo_microapp_sdk::email_threading`
//! so existing call sites keep compiling without churn. New
//! consumers should reach for the SDK path directly.
//!
//! Marketing-specific by design: nothing — pure pass-through.

pub use nexo_microapp_sdk::email_threading::{
    normalize_subject, synth_thread_id,
};

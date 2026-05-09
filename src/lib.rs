// `nexo-rs-extension-marketing` — installable email marketing
// extension for the nexo-rs framework.
//
// Subprocess plugin (Phase 81.5 contract). Subscribes to the
// email plugin's broker topics, resolves sender identity per
// tenant, routes inbound to sellers via YAML rules, generates
// AI draft replies the operator approves in the agent-creator
// microapp UI, schedules followups, tracks lead state.
//
// See `proyecto/PHASES-microapps.md` Phase 82.15 (wire shapes)
// + the agent-creator microapp's `proyecto/PHASES.md` M15 (the
// driving milestone) for full context.

#![warn(missing_docs)]

//! Email marketing extension for nexo-rs.
//!
//! See the README for installation + configuration. Public
//! surface is intentionally tiny: the binary is the entry
//! point, modules are wiring details.

pub mod admin;
pub mod audit;
pub mod availability;
pub mod broker;
pub mod compliance;
pub mod config;
pub mod draft;
pub mod draft_lock;
pub mod duplicate;
pub mod enrichment;
pub mod error;
pub mod firehose;
pub mod forwarder;
pub mod guardrails;
pub mod identity;
pub mod lead;
pub mod notification;
pub mod notification_dedup;
pub mod plugin;
pub mod scoring;
pub mod spam_filter;
pub mod tenant;
pub mod threading;
pub mod tools;
pub mod tracking;
pub mod whatsapp_ingest;

pub use error::MarketingError;
pub use tenant::{TenantId, TenantIdError};

/// Initialise process-wide tracing. Levels read from
/// `RUST_LOG`; defaults to `info` when unset. Idempotent —
/// safe to call multiple times.
///
/// Writes to **stderr** (NOT stdout) — when marketing runs as a
/// daemon-spawned plugin-child, stdout is the JSON-RPC channel.
/// Mixing log lines into it breaks the host's `initialize`
/// handshake. The daemon's `plugin.stderr` tracing target
/// captures stderr lines for operator visibility.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nexo_marketing=debug".into()),
        )
        .with_target(true)
        .with_writer(std::io::stderr)
        .try_init();
}

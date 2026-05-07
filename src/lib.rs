// `nexo-rs-extension-marketing` — installable email marketing
// extension for the nexo-rs framework.
//
// Subprocess plugin (Phase 81.5 contract). Subscribes to the
// email plugin's broker topics, resolves sender identity per
// tenant, routes inbound to vendedores via YAML rules, generates
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
pub mod broker;
pub mod compliance;
pub mod config;
pub mod enrichment;
pub mod error;
pub mod firehose;
pub mod identity;
pub mod lead;
pub mod plugin;
pub mod tenant;
pub mod tools;

pub use error::MarketingError;
pub use tenant::{TenantId, TenantIdError};

/// Initialise process-wide tracing. Levels read from
/// `RUST_LOG`; defaults to `info` when unset. Idempotent —
/// safe to call multiple times.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nexo_marketing=debug".into()),
        )
        .with_target(true)
        .try_init();
}

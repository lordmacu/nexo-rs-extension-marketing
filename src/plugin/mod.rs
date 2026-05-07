//! Plugin contract glue (Phase 81.5).
//!
//! Wires the extension's tool handlers + broker subscriber to
//! the SDK's `PluginAdapter` so the daemon can spawn the
//! `nexo-marketing` binary as a subprocess, hand it
//! `tool.invoke` calls over JSON-RPC stdio, and route
//! `plugin.inbound.email.*` events through to the inbound
//! decoder.
//!
//! Sibling pattern of `nexo-rs-plugin-browser/src/main.rs`:
//! a single dispatch closure routes by `tool_name`, and a
//! single broker handler decodes the email envelope. The
//! HTTP admin surface (operator UI proxy) keeps running in
//! a `tokio::spawn` alongside the stdio loop.

use std::sync::Arc;

use crate::lead::{LeadRouter, LeadStore};
use crate::tenant::TenantId;

pub mod broker;
pub mod dispatch;
pub mod tool_defs;

/// Shared dependencies the dispatch closure + broker handler
/// need at runtime. Cloned per-call (each field is `Arc`-cheap).
#[derive(Clone)]
pub struct PluginDeps {
    pub tenant_id: TenantId,
    pub lead_store: Arc<LeadStore>,
    pub router: Arc<LeadRouter>,
}

impl PluginDeps {
    pub fn new(
        tenant_id: TenantId,
        lead_store: Arc<LeadStore>,
        router: Arc<LeadRouter>,
    ) -> Self {
        Self {
            tenant_id,
            lead_store,
            router,
        }
    }
}

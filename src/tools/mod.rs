//! Tool handlers exposed via the plugin contract.
//!
//! Each handler matches the SDK signature
//! `async fn(Value, ToolCtx) -> Result<ToolReply, ToolError>`
//! so the binary's `Microapp::with_tool` registry mounts them
//! straight (Phase 81.5 plugin contract). Tools are advertised
//! in `nexo-plugin.toml` `[plugin.extends].tools` and the
//! daemon kills the handshake if any name diverges.
//!
//! Each tool delegates to the typed core (`lead`, `identity`,
//! `enrichment`) so the plugin layer stays thin.

use std::sync::Arc;

use crate::lead::LeadStore;
use crate::tenant::TenantId;

pub mod lead_detect_meeting_intent;
pub mod lead_followup_sweep;
pub mod lead_mark_qualified;
pub mod lead_profile;
pub mod lead_route;
pub mod lead_schedule_followup;

/// Dependencies the tool handlers share. Each field is
/// `Arc`-shareable so cloning per-call is cheap. The binary
/// instantiates one `ToolDeps` at boot + stamps it onto the
/// SDK's `ToolCtx` via `Microapp::with_state` (or the closure
/// capture pattern when SDK doesn't carry typed state yet).
#[derive(Clone)]
pub struct ToolDeps {
    pub lead_store: Arc<LeadStore>,
    pub tenant_id: TenantId,
}

impl ToolDeps {
    pub fn new(lead_store: Arc<LeadStore>, tenant_id: TenantId) -> Self {
        Self { lead_store, tenant_id }
    }
}

/// Names advertised in `nexo-plugin.toml [plugin.extends].tools`.
/// Source-of-truth so the binary's tool registration loop
/// can't drift from the manifest.
pub const TOOL_NAMES: &[&str] = &[
    "marketing_lead_profile",
    "marketing_lead_route",
    "marketing_lead_schedule_followup",
    "marketing_lead_mark_qualified",
    "marketing_lead_detect_meeting_intent",
    "marketing_lead_followup_sweep",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_match_manifest_count() {
        assert_eq!(TOOL_NAMES.len(), 6);
    }

    #[test]
    fn tool_names_unique() {
        let mut sorted: Vec<&&str> = TOOL_NAMES.iter().collect();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), TOOL_NAMES.len());
    }
}

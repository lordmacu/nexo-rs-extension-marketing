//! `tool.invoke` dispatch — routes the SDK's `ToolInvocation` to
//! the right `crate::tools::*::handle` and adapts the
//! `ToolReply / ToolError` surface to the plugin contract's
//! `Value / ToolInvocationError`.
//!
//! Sibling pattern of `nexo-rs-plugin-browser/src/dispatch.rs`:
//! a single match on `tool_name` keeps the routing trivially
//! auditable. Adding a tool here requires a paired
//! `ToolDef` entry in `tool_defs.rs` + a name in
//! `nexo-plugin.toml [plugin.extends].tools`.

use serde_json::Value;

use nexo_microapp_sdk::plugin::{ToolContext, ToolInvocation, ToolInvocationError};
use nexo_microapp_sdk::ToolError;

use crate::plugin::PluginDeps;
use crate::tools;

/// Map a microapp-SDK [`ToolError`] to the plugin contract's
/// [`ToolInvocationError`] band. The codes don't line up
/// 1:1 with the host's `-33401..-33405` decoder, so we pick the
/// closest semantic neighbour:
///
/// | `ToolError`           | `ToolInvocationError` |
/// |-----------------------|----------------------|
/// | `InvalidArguments(_)` | `ArgumentInvalid(_)` |
/// | `NotImplemented`      | `Unavailable("not implemented")` |
/// | `Internal(_)`         | `ExecutionFailed(_)` |
/// | `Unauthorized(_)`     | `Denied(_)` |
fn map_tool_error(e: ToolError) -> ToolInvocationError {
    match e {
        ToolError::InvalidArguments(s) => ToolInvocationError::ArgumentInvalid(s),
        ToolError::NotImplemented => {
            ToolInvocationError::Unavailable("not implemented".into())
        }
        ToolError::Internal(s) => ToolInvocationError::ExecutionFailed(s),
        ToolError::Unauthorized(s) => ToolInvocationError::Denied(s),
        // `ToolError` is `#[non_exhaustive]`. Future variants
        // surface as ExecutionFailed so the host pattern-match
        // for `-33403` keeps working — operator picks them up
        // through the message.
        other => ToolInvocationError::ExecutionFailed(other.to_string()),
    }
}

/// Route a `tool.invoke` to the right handler. Each handler
/// returns `Result<ToolReply, ToolError>`; we unwrap the
/// `ToolReply.value` into the JSON-RPC `result` field and map
/// errors per the table above.
///
/// Unknown tool names map to `NotFound` so the host can surface
/// the divergence between manifest + binary cleanly. The
/// manifest-vs-binary lockstep test in `tool_defs.rs` should
/// have caught it earlier — this is defense-in-depth.
pub async fn dispatch(
    deps: PluginDeps,
    inv: ToolInvocation,
    ctx: Option<&ToolContext>,
) -> Result<Value, ToolInvocationError> {
    let tenant = &deps.tenant_id;
    let result = match inv.tool_name.as_str() {
        "marketing_lead_profile" => tools::lead_profile::handle(tenant, inv.args).await,
        "marketing_lead_route" => {
            tools::lead_route::handle(tenant, deps.router.load_full(), inv.args).await
        }
        "marketing_lead_schedule_followup" => {
            tools::lead_schedule_followup::handle(tenant, deps.lead_store.clone(), inv.args).await
        }
        "marketing_lead_mark_qualified" => {
            // Phase 81.17.c.ctx — tool gets BrokerSender via
            // ToolContext, publishes LeadTransitioned
            // notification post-transition. Vendedor lookup
            // resolved from PluginDeps (live-reloaded by PUT
            // /config/vendedores).
            tools::lead_mark_qualified::handle(
                tenant,
                deps.lead_store.clone(),
                deps.vendedores.as_ref(),
                ctx.map(|c| &c.broker),
                inv.args,
            )
            .await
        }
        "marketing_lead_detect_meeting_intent" => {
            // Same pattern — high-confidence intents publish
            // a MeetingIntent notification.
            tools::lead_detect_meeting_intent::handle(
                tenant,
                deps.lead_store.clone(),
                deps.vendedores.as_ref(),
                ctx.map(|c| &c.broker),
                inv.args,
            )
            .await
        }
        "marketing_lead_followup_sweep" => {
            tools::lead_followup_sweep::handle(tenant, deps.lead_store.clone(), inv.args).await
        }
        other => return Err(ToolInvocationError::NotFound(other.into())),
    };

    result.map(|reply| reply.into_value()).map_err(map_tool_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Arc;

    use nexo_tool_meta::marketing::{RuleSet, TenantIdRef};
    use serde_json::json;

    use crate::lead::{router_handle, LeadRouter, LeadStore};
    use crate::tenant::TenantId;

    async fn deps_for(tenant_id: &str) -> PluginDeps {
        let tenant = TenantId::new(tenant_id).unwrap();
        let store = Arc::new(
            LeadStore::open(PathBuf::from(":memory:"), tenant.clone())
                .await
                .expect("open lead store"),
        );
        let rule_set = RuleSet {
            tenant_id: TenantIdRef(tenant.as_str().into()),
            version: 0,
            rules: Vec::new(),
            default_target: nexo_tool_meta::marketing::AssignTarget::Drop,
        };
        let router = router_handle(LeadRouter::new(tenant.clone(), rule_set));
        PluginDeps::new(tenant, store, router)
    }

    #[tokio::test]
    async fn unknown_tool_maps_to_notfound() {
        let deps = deps_for("acme").await;
        let inv = ToolInvocation {
            plugin_id: "marketing".into(),
            tool_name: "marketing_does_not_exist".into(),
            args: json!({}),
            agent_id: None,
        };
        let err = dispatch(deps, inv, None).await.unwrap_err();
        assert!(matches!(err, ToolInvocationError::NotFound(_)));
    }

    #[tokio::test]
    async fn invalid_args_maps_to_argument_invalid() {
        let deps = deps_for("acme").await;
        let inv = ToolInvocation {
            plugin_id: "marketing".into(),
            tool_name: "marketing_lead_profile".into(),
            // Missing required fields → InvalidArguments.
            args: json!({ "tenant_id": "acme" }),
            agent_id: None,
        };
        let err = dispatch(deps, inv, None).await.unwrap_err();
        assert!(
            matches!(err, ToolInvocationError::ArgumentInvalid(_)),
            "expected ArgumentInvalid, got {err:?}"
        );
    }

    #[tokio::test]
    async fn cross_tenant_call_returns_inline_error_not_denied() {
        // Defense-in-depth: the handlers stamp tenant_unauthorised
        // inside an `Ok(...)` reply (so the audit trail captures
        // the attempt) instead of an `Err`. Verify dispatch
        // preserves that contract — we DON'T want it surfaced
        // as `Denied` because tools/lead_profile.rs encodes the
        // cross-tenant case as a successful tool call with an
        // `ok: false` result, by design.
        let deps = deps_for("acme").await;
        let inv = ToolInvocation {
            plugin_id: "marketing".into(),
            tool_name: "marketing_lead_profile".into(),
            args: json!({
                "tenant_id": "other-tenant",
                "from_email": "x@y.com",
                "subject": "s",
                "body_excerpt": "b"
            }),
            agent_id: None,
        };
        let v = dispatch(deps, inv, None).await.expect("ok reply");
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "tenant_unauthorised");
    }

    #[tokio::test]
    async fn followup_sweep_wires_lead_store() {
        let deps = deps_for("acme").await;
        let inv = ToolInvocation {
            plugin_id: "marketing".into(),
            tool_name: "marketing_lead_followup_sweep".into(),
            args: json!({ "tenant_id": "acme", "limit": 10 }),
            agent_id: None,
        };
        let v = dispatch(deps, inv, None).await.expect("sweep should succeed");
        // Empty store → empty list, but the envelope is present.
        // Reply shape: `{ ok: true, result: { due: [], now_ms } }`.
        assert_eq!(v["ok"], true, "ok flag missing: {v}");
        assert!(
            v["result"]["due"].is_array(),
            "missing result.due array: {v}"
        );
    }
}

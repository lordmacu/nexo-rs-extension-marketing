//! `marketing_lead_route` tool — calls the per-tenant
//! `LeadRouter` and returns `LeadRouteResponse`.
//!
//! **F29 sweep:** marketing-specific by design. Agent-callable
//! tool over the (lifted) routing dispatcher.

use std::sync::Arc;

use serde_json::Value;

use nexo_microapp_sdk::{ToolError, ToolReply};
use nexo_tool_meta::marketing::{LeadRouteArgs, LeadRouteResponse, SellerId};

use crate::lead::{LeadRouter, RouteInputs, RouteOutcome};
use crate::tenant::TenantId;

pub async fn handle(
    expected_tenant: &TenantId,
    router: Arc<LeadRouter>,
    args: Value,
) -> Result<ToolReply, ToolError> {
    let parsed: LeadRouteArgs = serde_json::from_value(args)
        .map_err(|e| ToolError::InvalidArguments(format!("lead_route args: {e}")))?;

    if parsed.tenant_id.0 != expected_tenant.as_str() {
        return Ok(ToolReply::ok_json(serde_json::json!({
            "ok": false,
            "error": {
                "code": "tenant_unauthorised",
                "expected": expected_tenant.as_str(),
                "got": parsed.tenant_id.0,
            }
        })));
    }

    // The agent calls this tool AFTER the inbound has been
    // pre-processed (lead created, identity resolved, score
    // assigned). We assume the router was constructed with
    // route inputs already populated; for now the tool calls
    // the router with default-empty inputs to exercise the
    // dispatcher's "default target" path. Real wiring stamps
    // inputs from the lead store at call time.
    let outcome = router
        .route(&RouteInputs::default())
        .map_err(|e| ToolError::Internal(e.to_string()))?;

    let resp = match outcome {
        RouteOutcome::Seller {
            seller_id,
            matched_rule_id,
            why,
        } => LeadRouteResponse {
            seller_id: Some(seller_id),
            matched_rule_id,
            why_routed: why,
        },
        RouteOutcome::Drop {
            matched_rule_id,
            why,
        } => LeadRouteResponse {
            seller_id: None,
            matched_rule_id,
            why_routed: why,
        },
        RouteOutcome::NoTarget {
            matched_rule_id,
            why,
        } => LeadRouteResponse {
            seller_id: None,
            matched_rule_id,
            why_routed: why,
        },
    };

    Ok(ToolReply::ok_json(serde_json::json!({
        "ok": true,
        "result": resp,
    })))
}

// satisfy unused-import lint for type-only re-exports
#[allow(dead_code)]
fn _typecheck() -> Option<SellerId> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexo_microapp_sdk::routing::{AssignTarget, RuleSet, TenantIdRef};

    fn args(t: &str, lead: &str) -> Value {
        serde_json::json!({
            "tenant_id": t,
            "lead_id": lead,
        })
    }

    fn router_drop_default(t: &str) -> Arc<LeadRouter> {
        let rs = RuleSet {
            tenant_id: TenantIdRef(t.into()),
            version: 1,
            rules: vec![],
            default_target: AssignTarget::Drop,
        };
        Arc::new(LeadRouter::new(TenantId::new(t).unwrap(), rs))
    }

    #[tokio::test]
    async fn returns_drop_outcome_when_default_drop() {
        let r = handle(
            &TenantId::new("acme").unwrap(),
            router_drop_default("acme"),
            args("acme", "lead-1"),
        )
        .await
        .unwrap();
        let json = r.as_value();
        assert_eq!(json["ok"], true);
        assert!(json["result"]["seller_id"].is_null());
    }

    #[tokio::test]
    async fn tenant_mismatch_unauthorised() {
        let r = handle(
            &TenantId::new("acme").unwrap(),
            router_drop_default("acme"),
            args("globex", "lead-1"),
        )
        .await
        .unwrap();
        assert_eq!(r.as_value()["ok"], false);
    }
}

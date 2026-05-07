//! `marketing_lead_profile` tool handler.
//!
//! Takes (`tenant_id`, `from_email`, `subject`, `body_excerpt`)
//! and returns a `LeadProfileResponse` with the resolved
//! person + company + enrichment status. Hot path: cross-thread
//! linker hits → return immediately at confidence 0.95.
//!
//! The actual identity resolver wiring (chain composition +
//! adapter instantiation) happens at extension boot — this
//! handler validates args, ensures tenant scope matches, and
//! delegates. When the resolver isn't wired yet (current
//! state), the handler returns a low-confidence placeholder
//! so the agent can still proceed with the email's literal
//! sender info.

use serde_json::Value;

use nexo_microapp_sdk::{ToolError, ToolReply};
use nexo_tool_meta::marketing::{
    EnrichmentStatus, LeadProfileArgs, LeadProfileResponse, PersonId,
};

use crate::tenant::TenantId;

/// Validate args + (when wired) call into the identity
/// resolver. Tenant id must match the deps' tenant scope —
/// defense-in-depth so a misconfigured caller can't probe
/// other tenants through this tool.
///
/// The binary's tool-registration closure wraps this in a
/// `ToolCtx`-shaped handler so the SDK's `Microapp::with_tool`
/// signature is satisfied.
pub async fn handle(
    expected_tenant: &TenantId,
    args: Value,
) -> Result<ToolReply, ToolError> {
    let parsed: LeadProfileArgs = serde_json::from_value(args)
        .map_err(|e| ToolError::InvalidArguments(format!("lead_profile args: {e}")))?;

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
    if !parsed.from_email.contains('@') {
        return Err(ToolError::InvalidArguments(format!(
            "lead_profile: from_email missing '@': {:?}",
            parsed.from_email
        )));
    }

    // Placeholder response: the caller agent gets a stable
    // person_id derived from the email + a 'none' enrichment
    // marker so it knows it should ask for more context. The
    // real resolver lands when the binary wires the chain
    // (display_name + signature + LLM extractor + cross-thread
    // + reply_to) in main.rs.
    let person_id = derive_placeholder_person_id(&parsed.from_email);
    let resp = LeadProfileResponse {
        person_id,
        company_id: None,
        enrichment_status: EnrichmentStatus::None,
        enrichment_confidence: 0.0,
        merged_into_existing: false,
    };
    Ok(ToolReply::ok_json(serde_json::json!({
        "ok": true,
        "result": resp,
    })))
}

fn derive_placeholder_person_id(email: &str) -> PersonId {
    // Stable deterministic id from the email — UUID v5 over a
    // fixed namespace so callers re-running the tool with the
    // same email get the same id. When the real resolver
    // attaches, it overrides with the resolver's typed id.
    let ns = uuid::Uuid::NAMESPACE_DNS;
    let v5 = uuid::Uuid::new_v5(&ns, email.to_ascii_lowercase().as_bytes());
    PersonId(format!("placeholder-{v5}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexo_tool_meta::marketing::TenantIdRef;

    fn args(tenant: &str, email: &str) -> Value {
        serde_json::json!({
            "tenant_id": tenant,
            "from_email": email,
            "subject": "Hi",
            "body_excerpt": "hola",
        })
    }

    #[tokio::test]
    async fn happy_path_returns_placeholder_response() {
        let t = TenantId::new("acme").unwrap();
        let r = handle(&t, args("acme", "juan@acme.com")).await.unwrap();
        let json = r.as_value();
        assert_eq!(json["ok"], true);
        assert!(json["result"]["person_id"].as_str().unwrap().starts_with("placeholder-"));
    }

    #[tokio::test]
    async fn tenant_mismatch_returns_unauthorised() {
        let t = TenantId::new("acme").unwrap();
        let r = handle(&t, args("globex", "juan@globex.io")).await.unwrap();
        let json = r.as_value();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "tenant_unauthorised");
    }

    #[tokio::test]
    async fn malformed_email_invalid_arguments() {
        let t = TenantId::new("acme").unwrap();
        let err = handle(&t, args("acme", "no-at-symbol"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn deterministic_person_id_for_same_email() {
        let t = TenantId::new("acme").unwrap();
        let r1 = handle(&t, args("acme", "juan@acme.com")).await.unwrap();
        let r2 = handle(&t, args("acme", "JUAN@ACME.COM")).await.unwrap();
        let id1 = r1.as_value()["result"]["person_id"].clone();
        let id2 = r2.as_value()["result"]["person_id"].clone();
        assert_eq!(id1, id2);
    }

    #[allow(dead_code)] // satisfy `unused_imports` for type-only
    fn _typecheck_tenant_ref() -> TenantIdRef {
        TenantIdRef("a".into())
    }
}

//! Static [`ToolDef`] catalogue advertised to the daemon in the
//! `initialize` reply.
//!
//! Sibling pattern of `nexo-rs-plugin-browser/src/tool_defs.rs`:
//! every tool is hand-rolled here. Adding a tool requires editing
//! this file + `dispatch.rs` + `nexo-plugin.toml`
//! `[plugin.extends].tools` in lockstep — tests assert the count
//! matches `crate::tools::TOOL_NAMES`.

use nexo_microapp_sdk::plugin::ToolDef;
use serde_json::json;

/// Build the 6 marketing tool defs. Names must match
/// `nexo-plugin.toml [plugin.extends].tools` exactly — the
/// daemon kills the handshake if any name diverges.
pub fn marketing_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "marketing_lead_profile".into(),
            description: "Resolve sender identity for an inbound email. Returns the \
                resolved person + company + enrichment status. Hot path: cross-thread \
                linker hits → return at confidence 0.95. Cold senders go through the \
                fallback chain (display_name → signature → llm_extractor → cross_thread \
                → reply_to)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string", "description": "Operator's tenant id." },
                    "from_email": { "type": "string", "format": "email" },
                    "subject": { "type": "string" },
                    "body_excerpt": { "type": "string", "description": "First ~400 chars of the email body." }
                },
                "required": ["tenant_id", "from_email", "subject", "body_excerpt"]
            }),
        },
        ToolDef {
            name: "marketing_lead_route".into(),
            description: "Pick the vendedor to assign a lead to using the per-tenant \
                YAML rule set. Returns `vendedor_id` + the matched rule id. Falls back to \
                the tenant's `default_target` (round-robin or drop) when no rule matches."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string" },
                    "lead_id": { "type": "string", "description": "UUID of the lead to route." }
                },
                "required": ["tenant_id", "lead_id"]
            }),
        },
        ToolDef {
            name: "marketing_lead_schedule_followup".into(),
            description: "Schedule (or cancel) the next followup check for a lead. Pass \
                `next_check_at_ms = null` to cancel a pending followup (e.g., when the \
                client just replied). `bump_attempts: true` increments the followup \
                counter — pair with the cadence template's max attempts."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string" },
                    "lead_id": { "type": "string" },
                    "next_check_at_ms": {
                        "type": ["integer", "null"],
                        "description": "Absolute UTC ms timestamp. Null cancels."
                    },
                    "bump_attempts": { "type": "boolean", "default": false }
                },
                "required": ["tenant_id", "lead_id"]
            }),
        },
        ToolDef {
            name: "marketing_lead_mark_qualified".into(),
            description: "Transition a lead to `qualified` with an operator-supplied \
                reason. Validates the legal transition (current state → qualified) and \
                writes a `LeadTransitionEvent` to the audit log."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string" },
                    "lead_id": { "type": "string" },
                    "reason": { "type": "string", "description": "Audit-trail reason." }
                },
                "required": ["tenant_id", "lead_id"]
            }),
        },
        ToolDef {
            name: "marketing_lead_detect_meeting_intent".into(),
            description: "Cheap regex pass over the email body for meeting intent. \
                Returns `{ has_intent, confidence, kind, evidence_span }`. Calendly / \
                cal.com URLs → 0.90 confidence; ES+EN affirmation phrases ('sí, podemos \
                vernos el martes 15h') → 0.78. LLM upgrade stage lands when the \
                extractor backend is wired."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string" },
                    "body": { "type": "string" },
                    "history": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Last 3-5 thread messages, oldest first."
                    },
                    "lead_id": {
                        "type": "string",
                        "description": "Optional. When set + confidence ≥ 0.7, the tool publishes a MeetingIntent notification to agent.email.notification.<agent_id>."
                    }
                },
                "required": ["tenant_id", "body"]
            }),
        },
        ToolDef {
            name: "marketing_lead_followup_sweep".into(),
            description: "List leads whose `next_check_at_ms <= now`. Cron-friendly \
                pagination via `limit`. Returns the lead summary + computed `due_for` \
                (ms overdue) so the caller can prioritise."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tenant_id": { "type": "string" },
                    "now_ms": {
                        "type": ["integer", "null"],
                        "description": "Defaults to wall-clock UTC ms."
                    },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50 }
                },
                "required": ["tenant_id"]
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tools::TOOL_NAMES;

    #[test]
    fn tool_def_count_matches_manifest() {
        let defs = marketing_tool_defs();
        assert_eq!(defs.len(), TOOL_NAMES.len());
    }

    #[test]
    fn tool_def_names_match_manifest_exactly() {
        let defs = marketing_tool_defs();
        let def_names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        for expected in TOOL_NAMES {
            assert!(
                def_names.contains(expected),
                "manifest tool {expected} not advertised in tool_defs"
            );
        }
    }

    #[test]
    fn every_def_has_object_schema() {
        for def in marketing_tool_defs() {
            assert_eq!(
                def.input_schema["type"], "object",
                "tool {} input_schema must be 'object'",
                def.name
            );
        }
    }

    #[test]
    fn every_def_requires_tenant_id() {
        for def in marketing_tool_defs() {
            let req = def.input_schema["required"]
                .as_array()
                .unwrap_or_else(|| panic!("{} missing 'required'", def.name));
            assert!(
                req.iter().any(|v| v == "tenant_id"),
                "tool {} missing required tenant_id",
                def.name
            );
        }
    }
}

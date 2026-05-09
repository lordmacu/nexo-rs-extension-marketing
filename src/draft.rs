//! M15.21 slice 4 — pluggable draft generation.
//!
//! **F29 sweep:** marketing-specific by design. The Handlebars
//! sandbox + render helpers are already lifted to
//! `nexo-microapp-sdk::templating::handlebars`; this module is
//! the CRM-shaped consumer (DraftContext = lead + seller +
//! last_inbound + operator_hint, per-seller template override
//! reads `Seller::draft_template`). The trait seam itself
//! (`DraftGenerator`) is single-consumer today — lifting the
//! trait shape without a second impl would be premature.
//!
//! The operator-facing flow is a pull, not a push: the lead
//! drawer surfaces a "Generate AI draft" affordance and the
//! admin endpoint triggers a `DraftGenerator` to produce the
//! body. Generated rows land as
//! `direction = draft / status = pending` and pick up the
//! existing slice-1/2 pipeline (operator approves, slice-2
//! approve handler runs the compliance gate + publishes).
//!
//! ## Why a trait
//!
//! Two impls land in this crate:
//!
//! 1. [`TemplateDraftGenerator`] — sandboxed Handlebars (F26)
//!    rendering of an operator-configurable template against
//!    a structured [`DraftContext`]. Ships as the default —
//!    deterministic, no network, no API key, works offline.
//! 2. *Future* `AgentDraftGenerator` — RPC over NATS to the
//!    seller's bound agent (`agent.draft.request.<agent_id>`)
//!    where the agent uses its LLM to compose the body.
//!    Lands as a follow-up once the agent-side protocol is
//!    defined; the trait seam keeps the swap path clean.
//!
//! ## Sandbox + safety
//!
//! Templates flow through [`render_handlebars`] which
//! enforces no helpers / no partials / lenient missing keys.
//! See `nexo_microapp_sdk::templating::handlebars` for the
//! full sandbox guarantees.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use nexo_microapp_sdk::templating::handlebars::{
    render_handlebars, HandlebarsRenderError,
};
use nexo_tool_meta::marketing::{Lead, LeadState, Seller};

use crate::lead::{MessageDirection, ThreadMessage};

/// Structured context every generator receives. Keep this
/// stable — adding fields is fine, removing breaks operator
/// templates that referenced the dropped path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftContext {
    /// The lead the draft replies to. Subject + state +
    /// score + topic_tags are the most-templated fields.
    pub lead: Lead,
    /// The seller assigned to the lead. `name` +
    /// `signature_text` typically anchor the closing of the
    /// draft body; `preferred_language` informs tone.
    pub seller: Seller,
    /// The most recent inbound message body (what the lead
    /// actually wrote). `None` when the thread carries no
    /// inbound rows yet — placeholder leads + cold outbound
    /// where marketing initiates the conversation.
    pub last_inbound: Option<ThreadMessage>,
    /// Full chronological history of the thread (oldest first):
    /// inbound + outbound + previously-approved drafts. The LLM
    /// generator turns this into a multi-turn prompt so replies
    /// reflect the entire conversation, not just the latest
    /// message. Empty when the lead is brand-new. Capped at the
    /// caller (typically 50 messages) to keep token budget sane;
    /// older messages get summarised at the call site when
    /// truncation is needed.
    #[serde(default)]
    pub thread_history: Vec<ThreadMessage>,
    /// Optional operator hint passed in the generate request
    /// body. Lets the operator nudge the generator
    /// ("focus on pricing", "mention the demo on Friday")
    /// without editing the template. The default template
    /// renders this verbatim when present.
    #[serde(default)]
    pub operator_hint: Option<String>,
}

/// Errors a generator can return. Caller maps to typed HTTP
/// responses so the operator UI surfaces actionable codes
/// (`template_invalid`, `generator_unavailable`).
#[derive(Debug, thiserror::Error)]
pub enum DraftGenError {
    /// The template body itself is malformed (Handlebars
    /// parse failure on a `{{#if}}` / etc.).
    #[error("template parse error: {0}")]
    TemplateParse(String),
    /// The template compiled but rendering blew up against
    /// the context (forbidden helper invocation, etc.).
    #[error("template render error: {0}")]
    TemplateRender(String),
    /// The generator backend (LLM API / agent NATS / etc.)
    /// refused or didn't respond. Operator retries when this
    /// fires — the lead drawer surfaces the message inline.
    #[error("generator backend unavailable: {0}")]
    Backend(String),
    /// The generator produced an empty body. Trimmed length
    /// is zero — refuse so we don't persist a blank draft
    /// the approve handler would later refuse anyway.
    #[error("generator returned an empty body")]
    Empty,
}

impl From<HandlebarsRenderError> for DraftGenError {
    fn from(e: HandlebarsRenderError) -> Self {
        match e {
            HandlebarsRenderError::Parse(m) => Self::TemplateParse(m),
            HandlebarsRenderError::Render(m) => Self::TemplateRender(m),
        }
    }
}

/// The pluggable generator surface. One method, async,
/// returns the proposed draft body verbatim — the caller
/// (the admin handler) wraps the result into a
/// `NewThreadMessage` + persists.
#[async_trait]
pub trait DraftGenerator: Send + Sync {
    /// Generate a draft body against `ctx`. The returned
    /// string is the message body; the admin handler stamps
    /// `direction = draft / status = pending` + the
    /// `from_label` (defaults to "AI") around it.
    async fn generate(&self, ctx: &DraftContext) -> Result<String, DraftGenError>;
}

/// Default template the [`TemplateDraftGenerator`] ships
/// with when the operator hasn't authored a custom one.
/// Spanish-leaning since the project's default operator
/// language is Spanish — operators on English-only deals
/// override per tenant.
pub const DEFAULT_TEMPLATE: &str = r#"Hola{{#if last_inbound.from_label}} {{last_inbound.from_label}}{{/if}},

Gracias por tu mensaje sobre "{{lead.subject}}".
{{#if last_inbound.body}}
Recibimos tu nota:
> {{last_inbound.body}}
{{/if}}
{{#if operator_hint}}
{{operator_hint}}

{{/if}}Quedamos atentos para coordinar los siguientes pasos.

Saludos,
{{seller.name}}{{#if seller.signature_text}}

{{seller.signature_text}}{{/if}}
"#;

/// Hot-swappable handle for the active draft template.
/// Same `Arc` lives in the [`TemplateDraftGenerator`] and
/// the admin endpoint that serves
/// `PUT /config/draft_template`; the endpoint stores a
/// freshly-validated template and the next generator call
/// picks it up without a process restart.
pub type DraftTemplateHandle = std::sync::Arc<arc_swap::ArcSwap<String>>;

/// Construct a fresh template handle pre-loaded with the
/// bundled [`DEFAULT_TEMPLATE`]. Boot wires this once and
/// shares the Arc.
pub fn default_template_handle() -> DraftTemplateHandle {
    std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
        DEFAULT_TEMPLATE.to_string(),
    ))
}

/// Sandboxed-Handlebars draft generator. Reads the active
/// template through an [`DraftTemplateHandle`] so the
/// admin endpoint can hot-swap the per-tenant template
/// without restarting the extension.
#[derive(Debug, Clone)]
pub struct TemplateDraftGenerator {
    template: DraftTemplateHandle,
}

impl TemplateDraftGenerator {
    /// Construct against a shared handle. Caller wires the
    /// same Arc into AdminState so PUT updates flow
    /// straight into the generator's read path.
    pub fn from_handle(template: DraftTemplateHandle) -> Self {
        Self { template }
    }

    /// Construct with an explicit template body. Useful
    /// for tests + one-off setups; mutations won't reach
    /// any other component because the handle is local.
    pub fn new(template: String) -> Self {
        Self {
            template: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                template,
            )),
        }
    }

    /// Construct with the bundled [`DEFAULT_TEMPLATE`].
    /// Convenient for boot wiring before per-tenant
    /// templates exist.
    pub fn with_default_template() -> Self {
        Self::new(DEFAULT_TEMPLATE.into())
    }

    /// Snapshot of the currently active template body.
    /// Loads through `arc-swap` so concurrent writers
    /// don't see torn reads.
    pub fn template(&self) -> std::sync::Arc<String> {
        self.template.load_full()
    }

    /// The shared handle, for callers that need to wire it
    /// into AdminState alongside the generator itself.
    pub fn handle(&self) -> DraftTemplateHandle {
        self.template.clone()
    }
}

/// Fixture `DraftContext` used by the
/// `PUT /config/draft_template` validator to sandbox-
/// render an operator-typed template before persisting.
/// Carries every field a real draft generation hits so
/// operator-typed paths fail validation if they reference
/// non-existent fields.
pub fn validation_fixture_context() -> serde_json::Value {
    use chrono::Utc;
    use nexo_tool_meta::marketing::{
        IntentClass, LeadId, LeadState, PersonId, Seller, SellerId,
        SentimentBand, TenantIdRef,
    };
    let lead = nexo_tool_meta::marketing::Lead {
        id: LeadId("validation".into()),
        tenant_id: TenantIdRef("validation".into()),
        thread_id: "<validation@local>".into(),
        subject: "Subject preview".into(),
        person_id: PersonId("preview@example.com".into()),
        seller_id: SellerId("seller-preview".into()),
        state: LeadState::Engaged,
        score: 50,
        sentiment: SentimentBand::Neutral,
        intent: IntentClass::Comparing,
        topic_tags: vec!["preview".into()],
        last_activity_ms: Utc::now().timestamp_millis(),
        next_check_at_ms: None,
        followup_attempts: 0,
        why_routed: vec!["validation".into()],
        operator_notes: None,
    };
    let seller = Seller {
        id: SellerId("seller-preview".into()),
        tenant_id: TenantIdRef("validation".into()),
        name: "Seller Preview".into(),
        primary_email: "seller@example.com".into(),
        alt_emails: vec![],
        signature_text: "—\nSeller Preview".into(),
        working_hours: None,
        on_vacation: false,
        vacation_until: None,
        preferred_language: Some("es".into()),
        agent_id: None,
        model_override: None,
        notification_settings: None,
        smtp_credential: None,
        system_prompt: None,
        model_provider: None,
        model_id: None,
        draft_template: None,
    };
    let inbound = crate::lead::ThreadMessage {
        id: "preview-msg".into(),
        direction: crate::lead::MessageDirection::Inbound,
        from_label: "Preview".into(),
        body: "Preview body".into(),
        at_ms: Utc::now().timestamp_millis(),
        draft_status: None,
        subject: None,
    };
    let ctx = DraftContext {
        lead,
        seller,
        last_inbound: Some(inbound.clone()),
        thread_history: vec![inbound],
        operator_hint: Some("Preview hint".into()),
    };
    serde_json::to_value(&ctx).unwrap_or_else(|_| serde_json::json!({}))
}

#[async_trait]
impl DraftGenerator for TemplateDraftGenerator {
    async fn generate(&self, ctx: &DraftContext) -> Result<String, DraftGenError> {
        // Serialise the structured context into JSON so
        // Handlebars can walk the field tree. `serde_json`
        // here is the same shape `Seller` + `Lead` already
        // expose to the wire so operator templates can
        // reference the same paths the API returns.
        let context = serde_json::to_value(ctx)
            .map_err(|e| DraftGenError::TemplateRender(e.to_string()))?;
        // M15.21.seller-template — per-seller override wins
        // when set + non-empty. The tenant-level template
        // (hot-swappable handle) is the fallback. Empty
        // string is treated as "inherit" so the editor's
        // clear-to-fall-back UX matches the field semantics.
        let body = match ctx.seller.draft_template.as_deref() {
            Some(s) if !s.trim().is_empty() => std::sync::Arc::new(s.to_string()),
            _ => self.template.load_full(),
        };
        let rendered = render_handlebars(&body, &context)?;
        let trimmed = rendered.trim();
        if trimmed.is_empty() {
            return Err(DraftGenError::Empty);
        }
        Ok(rendered)
    }
}

/// Phase 82.10.t.x — LLM-driven draft generator.
///
/// Reads the seller's denormalised `system_prompt` + `ModelRef`
/// (stamped by the operator microapp at PUT-time from the bound
/// agent), builds a multi-turn chat from `ctx.thread_history`,
/// and calls `BrokerSender::complete_llm` so the daemon executes
/// against its configured providers + secrets. The seller's
/// `model_override`, when set, beats the agent default.
///
/// On any failure path (missing fields, broker unavailable, LLM
/// error) the generator falls back to the bundled
/// [`TemplateDraftGenerator`] so the operator always gets a
/// pending draft to review — the agent draft is a quality
/// improvement, not a hard gate.
pub struct AgentDraftGenerator {
    /// Hot-swap cell that the broker-event handler stores into
    /// every time the daemon delivers an event. Reading
    /// `load_full()` at generate time gives us the latest
    /// sender without locking around a long-lived handle.
    broker_cell: std::sync::Arc<
        arc_swap::ArcSwapOption<nexo_microapp_sdk::plugin::BrokerSender>,
    >,
    /// Fallback when the broker isn't connected yet (boot race)
    /// or the LLM call errors.
    fallback: TemplateDraftGenerator,
}

impl AgentDraftGenerator {
    /// Wire at boot. The `broker_cell` is the same `ArcSwapOption`
    /// the broker-event closure populates; the `fallback` is the
    /// bundled TemplateDraftGenerator (or any other impl).
    pub fn new(
        broker_cell: std::sync::Arc<
            arc_swap::ArcSwapOption<nexo_microapp_sdk::plugin::BrokerSender>,
        >,
        fallback: TemplateDraftGenerator,
    ) -> Self {
        Self {
            broker_cell,
            fallback,
        }
    }

    /// Resolve the (provider, model) the LLM call should target.
    /// Operator-set `model_override` wins; otherwise the bound
    /// agent's denormalised `model_provider` + `model_id`. Returns
    /// `None` when neither is set — caller falls back to the
    /// template generator.
    fn resolve_model(seller: &nexo_tool_meta::marketing::Seller) -> Option<(String, String)> {
        if let Some(over) = seller.model_override.as_ref() {
            if !over.provider.is_empty() && !over.model.is_empty() {
                return Some((over.provider.clone(), over.model.clone()));
            }
        }
        match (seller.model_provider.as_ref(), seller.model_id.as_ref()) {
            (Some(p), Some(m)) if !p.is_empty() && !m.is_empty() => {
                Some((p.clone(), m.clone()))
            }
            _ => None,
        }
    }
}

#[async_trait]
impl DraftGenerator for AgentDraftGenerator {
    async fn generate(&self, ctx: &DraftContext) -> Result<String, DraftGenError> {
        let Some(broker) = self.broker_cell.load_full() else {
            tracing::warn!(
                target: "extension.marketing.draft_generate",
                "broker sender not yet wired; falling back to template generator"
            );
            return self.fallback.generate(ctx).await;
        };
        let Some((provider, model)) = Self::resolve_model(&ctx.seller) else {
            tracing::warn!(
                target: "extension.marketing.draft_generate",
                seller_id = %ctx.seller.id.0,
                "seller has no model_override and no denormalised agent ModelRef; falling back to template"
            );
            return self.fallback.generate(ctx).await;
        };
        let system_prompt = match ctx.seller.system_prompt.as_deref() {
            Some(s) if !s.trim().is_empty() => Some(build_system_prompt(s, ctx)),
            _ => Some(build_system_prompt("", ctx)),
        };

        // Convert thread_history into chat messages.
        // Inbound  → role "user"
        // Outbound → role "assistant"
        // Drafts (any status) → skip — we don't want to
        //   replay our own pending suggestions back as
        //   model-authored history.
        let mut messages: Vec<nexo_llm::types::ChatMessage> = Vec::new();
        for tm in ctx.thread_history.iter() {
            let role = match tm.direction {
                crate::lead::MessageDirection::Inbound => nexo_llm::types::ChatRole::User,
                crate::lead::MessageDirection::Outbound => {
                    nexo_llm::types::ChatRole::Assistant
                }
                crate::lead::MessageDirection::Draft => continue,
            };
            // Compose a brief "From: <label>" prefix on inbound
            // so the model knows who's writing when the thread
            // has multiple participants. Outbound bodies stay
            // unwrapped — the assistant's own past replies.
            let content = match role {
                nexo_llm::types::ChatRole::User if !tm.from_label.is_empty() => {
                    format!("From: {}\n\n{}", tm.from_label, tm.body)
                }
                _ => tm.body.clone(),
            };
            messages.push(nexo_llm::types::ChatMessage {
                role,
                content,
                tool_call_id: None,
                name: None,
                tool_calls: vec![],
                attachments: vec![],
            });
        }
        // Operator hint goes as a final user-side instruction so
        // the LLM treats it as guidance about the next reply.
        if let Some(hint) = ctx.operator_hint.as_deref() {
            if !hint.trim().is_empty() {
                messages.push(nexo_llm::types::ChatMessage {
                    role: nexo_llm::types::ChatRole::User,
                    content: format!("[Operator hint for this reply]\n{hint}"),
                    tool_call_id: None,
                    name: None,
                    tool_calls: vec![],
                    attachments: vec![],
                });
            }
        }
        // Empty thread + no hint — anchor on a single user
        // message so the model has SOMETHING to reply to.
        if messages.is_empty() {
            messages.push(nexo_llm::types::ChatMessage {
                role: nexo_llm::types::ChatRole::User,
                content: format!(
                    "Draft an initial outreach message for this lead. Subject: {}",
                    ctx.lead.subject
                ),
                tool_call_id: None,
                name: None,
                tool_calls: vec![],
                attachments: vec![],
            });
        }

        let params = nexo_microapp_sdk::plugin::LlmCompleteParams {
            provider,
            model,
            messages,
            max_tokens: Some(1024),
            temperature: Some(0.7),
            system_prompt,
        };
        match broker.complete_llm(params).await {
            Ok(r) => {
                let body = r.content.trim();
                if body.is_empty() {
                    tracing::warn!(
                        target: "extension.marketing.draft_generate",
                        seller_id = %ctx.seller.id.0,
                        "LLM returned empty body; falling back to template"
                    );
                    return self.fallback.generate(ctx).await;
                }
                Ok(body.to_string())
            }
            Err(e) => {
                tracing::warn!(
                    target: "extension.marketing.draft_generate",
                    seller_id = %ctx.seller.id.0,
                    error = ?e,
                    "LLM call failed; falling back to template"
                );
                self.fallback.generate(ctx).await
            }
        }
    }
}

/// Compose a system prompt from the agent's `system_prompt`
/// (denormalised onto the seller) plus a small marketing
/// addendum that anchors output style. The addendum is short
/// on purpose — most of the persona steering (including
/// language) comes from the agent's own prompt; we only add
/// constraints specific to the email-reply use case.
///
/// All framing strings are English so the codebase stays
/// consistent with the framework's "code in English" rule;
/// the agent's `system_prompt` (operator-authored) decides
/// the output language and tone via its own copy.
fn build_system_prompt(agent_prompt: &str, ctx: &DraftContext) -> String {
    let mut out = String::new();
    if !agent_prompt.trim().is_empty() {
        out.push_str(agent_prompt.trim());
        out.push_str("\n\n");
    }
    out.push_str(
        "You are drafting an email reply to a lead. Return ONLY the message \
         body — no subject line, no headers, no extra signature (the seller's \
         signature is appended downstream). Keep the tone natural, concise, \
         and professional. One question or call-to-action per reply. Never \
         invent prices, exact dates, or commitments the seller would have to \
         honour. Reply in the same language as the most recent inbound \
         message unless the agent persona above pins a different language.",
    );
    if !ctx.seller.signature_text.is_empty() {
        out.push_str(&format!(
            "\n\nSeller signature (do NOT echo this — the system appends it): {}",
            ctx.seller.signature_text.replace('\n', " | "),
        ));
    }
    if !ctx.lead.topic_tags.is_empty() {
        out.push_str(&format!(
            "\n\nThread topic tags: {}.",
            ctx.lead.topic_tags.join(", "),
        ));
    }
    out
}

// ── Idempotent-draft signature ───────────────────────────────────
//
// Pure-fn over the input identity tuple a draft generation
// depends on. Same tuple ⇒ same LLM output (modulo temperature
// noise we ignore for dedup). The caller (admin handler + the
// followup_sweep tool) computes the signature, looks it up in
// the per-tenant store, and only spends tokens on a fresh
// generate when the lookup misses.
//
// Inputs covered:
// - lead.id           — different lead, different draft
// - lead.state        — state transitions invalidate prior drafts
// - last_inbound msg id — new inbound = new context
// - seller.id         — different seller may sign differently
// - seller.agent_id   — different agent persona
// - prompt_hash       — agent system prompt + draft template +
//                       model provider/id all baked into the
//                       hash so a tenant edit on any of them
//                       triggers regeneration

/// Stable label for `LeadState` used inside the signature.
/// Hardcoded here (vs `Display`) so a future rename of the
/// `LeadState` enum doesn't silently change every signature
/// already on disk.
fn lead_state_label(state: &LeadState) -> &'static str {
    match state {
        LeadState::Cold => "cold",
        LeadState::Engaged => "engaged",
        LeadState::MeetingScheduled => "meeting_scheduled",
        LeadState::Qualified => "qualified",
        LeadState::Lost => "lost",
    }
}

/// Hex-encoded sha256 over the draft generator's input
/// identity. Returns 64 lowercase hex chars.
///
/// `last_inbound_message_id` is `None` for cold leads with no
/// inbound row yet — empty string is folded into the hash so
/// the cold-no-inbound state has a stable signature too.
pub fn compute_draft_signature(
    lead: &Lead,
    last_inbound_message_id: Option<&str>,
    seller: &Seller,
) -> String {
    let mut prompt_hasher = Sha256::new();
    prompt_hasher.update(seller.system_prompt.as_deref().unwrap_or("").as_bytes());
    prompt_hasher.update(b"\x1f");
    prompt_hasher.update(seller.draft_template.as_deref().unwrap_or("").as_bytes());
    prompt_hasher.update(b"\x1f");
    prompt_hasher.update(seller.model_provider.as_deref().unwrap_or("").as_bytes());
    prompt_hasher.update(b"\x1f");
    prompt_hasher.update(seller.model_id.as_deref().unwrap_or("").as_bytes());
    let prompt_hash = format!("{:x}", prompt_hasher.finalize());

    let mut h = Sha256::new();
    h.update(lead.id.0.as_bytes());
    h.update(b"|");
    h.update(lead_state_label(&lead.state).as_bytes());
    h.update(b"|");
    h.update(last_inbound_message_id.unwrap_or("").as_bytes());
    h.update(b"|");
    h.update(seller.id.0.as_bytes());
    h.update(b"|");
    h.update(seller.agent_id.as_deref().unwrap_or("").as_bytes());
    h.update(b"|");
    h.update(prompt_hash.as_bytes());
    format!("{:x}", h.finalize())
}

/// Convenience: pull the most-recent inbound message id from a
/// thread history (oldest-first) so the caller doesn't repeat
/// the iter pattern at every callsite. Returns `None` when the
/// thread carries no inbound row yet (cold leads).
pub fn last_inbound_message_id(history: &[ThreadMessage]) -> Option<&str> {
    history
        .iter()
        .rev()
        .find(|m| m.direction == MessageDirection::Inbound)
        .map(|m| m.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nexo_tool_meta::marketing::{
        IntentClass, LeadId, LeadState, PersonId, SellerId, SentimentBand,
        TenantIdRef,
    };

    fn lead_fixture() -> Lead {
        Lead {
            id: LeadId("lead-1".into()),
            tenant_id: TenantIdRef("acme".into()),
            thread_id: "<msg-1@acme.com>".into(),
            subject: "Cotización planes Pro".into(),
            person_id: PersonId("juan@acme.com".into()),
            seller_id: SellerId("pedro".into()),
            state: LeadState::Engaged,
            score: 65,
            sentiment: SentimentBand::Positive,
            intent: IntentClass::Comparing,
            topic_tags: vec!["pricing".into()],
            last_activity_ms: Utc::now().timestamp_millis(),
            next_check_at_ms: None,
            followup_attempts: 0,
            why_routed: vec!["rule:warm-corp".into()],
            operator_notes: None,
        }
    }

    fn seller_fixture() -> Seller {
        Seller {
            id: SellerId("pedro".into()),
            tenant_id: TenantIdRef("acme".into()),
            name: "Pedro García".into(),
            primary_email: "pedro@acme.com".into(),
            alt_emails: vec![],
            signature_text: "—\nPedro · Acme".into(),
            working_hours: None,
            on_vacation: false,
            vacation_until: None,
            preferred_language: Some("es".into()),
            agent_id: None,
            model_override: None,
            notification_settings: None,
            smtp_credential: None,
            draft_template: None,
            system_prompt: None,
            model_provider: None,
            model_id: None,
        }
    }

    fn inbound_fixture(body: &str, from_label: &str) -> ThreadMessage {
        ThreadMessage {
            id: "msg-1".into(),
            direction: crate::lead::MessageDirection::Inbound,
            from_label: from_label.into(),
            body: body.into(),
            at_ms: Utc::now().timestamp_millis(),
            draft_status: None,
            subject: None,
        }
    }

    /// M15.21.seller-template — when the seller carries a
    /// non-empty `draft_template` override, the renderer uses
    /// it INSTEAD of the tenant-level handle.
    #[tokio::test]
    async fn seller_draft_template_override_wins_over_tenant() {
        let gen = TemplateDraftGenerator::with_default_template();
        let mut seller = seller_fixture();
        seller.draft_template =
            Some("Solo seller template: {{seller.name}}.".into());
        let ctx = DraftContext {
            lead: lead_fixture(),
            seller,
            last_inbound: None,
thread_history: vec![],
            operator_hint: None,
        };
        let body = gen.generate(&ctx).await.unwrap();
        assert_eq!(body, "Solo seller template: Pedro García.");
    }

    /// M15.21.seller-template — `Some("")` inherits the tenant
    /// template (whitespace-only treated the same way) so the
    /// editor's clear-to-fallback UX behaves predictably.
    #[tokio::test]
    async fn seller_empty_draft_template_inherits_tenant() {
        let gen = TemplateDraftGenerator::with_default_template();
        let mut seller = seller_fixture();
        seller.draft_template = Some("   \n   ".into());
        let ctx = DraftContext {
            lead: lead_fixture(),
            seller,
            last_inbound: Some(inbound_fixture(
                "¿Cuánto cuesta el plan Pro?",
                "Juan",
            )),
            thread_history: vec![],
            operator_hint: None,
        };
        let body = gen.generate(&ctx).await.unwrap();
        // Tenant default rendered — operator-friendly fingerprint.
        assert!(body.contains("Hola Juan,"));
        assert!(body.contains("Pedro García"));
    }

    /// M15.21.seller-template — malformed override surfaces
    /// the same `TemplateParse` / `TemplateRender` errors as a
    /// malformed tenant template (no special-case bypass).
    #[tokio::test]
    async fn seller_draft_template_malformed_surfaces_render_error() {
        let gen = TemplateDraftGenerator::with_default_template();
        let mut seller = seller_fixture();
        seller.draft_template = Some("{{#if".into());
        let ctx = DraftContext {
            lead: lead_fixture(),
            seller,
            last_inbound: None,
thread_history: vec![],
            operator_hint: None,
        };
        let err = gen.generate(&ctx).await.unwrap_err();
        assert!(
            matches!(
                err,
                DraftGenError::TemplateParse(_) | DraftGenError::TemplateRender(_),
            ),
            "{err:?}",
        );
    }

    #[tokio::test]
    async fn default_template_renders_with_full_context() {
        let gen = TemplateDraftGenerator::with_default_template();
        let ctx = DraftContext {
            lead: lead_fixture(),
            seller: seller_fixture(),
            last_inbound: Some(inbound_fixture(
                "¿Cuánto cuesta el plan Pro?",
                "Juan",
            )),
            thread_history: vec![],
            operator_hint: None,
        };
        let body = gen.generate(&ctx).await.unwrap();
        assert!(body.contains("Hola Juan,"));
        assert!(body.contains("Cotización planes Pro"));
        assert!(body.contains("Pedro García"));
        assert!(body.contains("—\nPedro · Acme"));
        assert!(body.contains("¿Cuánto cuesta el plan Pro?"));
    }

    #[tokio::test]
    async fn default_template_omits_inbound_block_when_no_inbound() {
        let gen = TemplateDraftGenerator::with_default_template();
        let ctx = DraftContext {
            lead: lead_fixture(),
            seller: seller_fixture(),
            last_inbound: None,
thread_history: vec![],
            operator_hint: None,
        };
        let body = gen.generate(&ctx).await.unwrap();
        // Generic "Hola," without a name when no inbound.
        assert!(body.contains("Hola,"));
        // No quoted reply when there's nothing to quote.
        assert!(!body.contains("Recibimos tu nota:"));
    }

    #[tokio::test]
    async fn default_template_renders_operator_hint() {
        let gen = TemplateDraftGenerator::with_default_template();
        let ctx = DraftContext {
            lead: lead_fixture(),
            seller: seller_fixture(),
            last_inbound: None,
thread_history: vec![],
            operator_hint: Some("Mencionar la promo de mayo".into()),
        };
        let body = gen.generate(&ctx).await.unwrap();
        assert!(body.contains("Mencionar la promo de mayo"));
    }

    #[tokio::test]
    async fn custom_template_renders_per_tenant() {
        let gen = TemplateDraftGenerator::new(
            "Subject: {{lead.subject}}\nFrom: {{seller.primary_email}}\n"
                .into(),
        );
        let ctx = DraftContext {
            lead: lead_fixture(),
            seller: seller_fixture(),
            last_inbound: None,
thread_history: vec![],
            operator_hint: None,
        };
        let body = gen.generate(&ctx).await.unwrap();
        assert_eq!(
            body,
            "Subject: Cotización planes Pro\nFrom: pedro@acme.com\n",
        );
    }

    #[tokio::test]
    async fn malformed_template_returns_parse_error() {
        // Unclosed `{{#if}}` block — the handlebars parser
        // refuses immediately.
        let gen = TemplateDraftGenerator::new(
            "Hola {{#if seller.name}}{{seller.name}}".into(),
        );
        let ctx = DraftContext {
            lead: lead_fixture(),
            seller: seller_fixture(),
            last_inbound: None,
thread_history: vec![],
            operator_hint: None,
        };
        let err = gen.generate(&ctx).await.unwrap_err();
        assert!(matches!(
            err,
            DraftGenError::TemplateParse(_) | DraftGenError::TemplateRender(_)
        ));
    }

    #[tokio::test]
    async fn whitespace_only_template_returns_empty_error() {
        let gen = TemplateDraftGenerator::new("\n   \n\t".into());
        let ctx = DraftContext {
            lead: lead_fixture(),
            seller: seller_fixture(),
            last_inbound: None,
thread_history: vec![],
            operator_hint: None,
        };
        let err = gen.generate(&ctx).await.unwrap_err();
        assert!(matches!(err, DraftGenError::Empty));
    }

    #[tokio::test]
    async fn missing_field_renders_empty_no_crash() {
        // F26 sandbox guarantee: typo in path renders empty
        // (lenient mode) rather than crashing the pipeline.
        let gen =
            TemplateDraftGenerator::new("Vendor: [{{seller.naem}}]".into());
        let ctx = DraftContext {
            lead: lead_fixture(),
            seller: seller_fixture(),
            last_inbound: None,
thread_history: vec![],
            operator_hint: None,
        };
        let body = gen.generate(&ctx).await.unwrap();
        assert_eq!(body, "Vendor: []");
    }

    #[tokio::test]
    async fn helpers_and_partials_are_blocked() {
        // F26 sandbox: no custom helpers fire (and no
        // partials are registered). The literal `{{> something}}`
        // partial reference fails — `unknown_helper` doesn't
        // resolve. Caller surfaces the typed error.
        let gen = TemplateDraftGenerator::new(
            "{{> shared/header }}\nHola {{seller.name}}".into(),
        );
        let ctx = DraftContext {
            lead: lead_fixture(),
            seller: seller_fixture(),
            last_inbound: None,
thread_history: vec![],
            operator_hint: None,
        };
        let err = gen.generate(&ctx).await.unwrap_err();
        assert!(matches!(
            err,
            DraftGenError::TemplateParse(_) | DraftGenError::TemplateRender(_)
        ));
    }

    // ── Idempotent-draft signature ──────────────────────────────

    #[test]
    fn signature_stable_for_identical_inputs() {
        let lead = lead_fixture();
        let seller = seller_fixture();
        let s1 = compute_draft_signature(&lead, Some("msg-1"), &seller);
        let s2 = compute_draft_signature(&lead, Some("msg-1"), &seller);
        assert_eq!(s1, s2);
        assert_eq!(s1.len(), 64); // sha256 hex
    }

    #[test]
    fn signature_changes_when_state_changes() {
        let mut lead = lead_fixture();
        let seller = seller_fixture();
        let s_engaged = compute_draft_signature(&lead, Some("msg-1"), &seller);
        lead.state = LeadState::Cold;
        let s_cold = compute_draft_signature(&lead, Some("msg-1"), &seller);
        assert_ne!(s_engaged, s_cold);
    }

    #[test]
    fn signature_changes_when_last_inbound_changes() {
        let lead = lead_fixture();
        let seller = seller_fixture();
        let s1 = compute_draft_signature(&lead, Some("msg-1"), &seller);
        let s2 = compute_draft_signature(&lead, Some("msg-2"), &seller);
        let s_none = compute_draft_signature(&lead, None, &seller);
        assert_ne!(s1, s2);
        assert_ne!(s1, s_none);
    }

    #[test]
    fn signature_changes_when_seller_agent_changes() {
        let lead = lead_fixture();
        let mut seller = seller_fixture();
        let s_no_agent = compute_draft_signature(&lead, Some("msg-1"), &seller);
        seller.agent_id = Some("aana".into());
        let s_aana = compute_draft_signature(&lead, Some("msg-1"), &seller);
        seller.agent_id = Some("bobo".into());
        let s_bobo = compute_draft_signature(&lead, Some("msg-1"), &seller);
        assert_ne!(s_no_agent, s_aana);
        assert_ne!(s_aana, s_bobo);
    }

    #[test]
    fn signature_changes_when_prompt_changes() {
        let lead = lead_fixture();
        let mut seller = seller_fixture();
        seller.system_prompt = Some("You are a helpful assistant.".into());
        let s_v1 = compute_draft_signature(&lead, Some("msg-1"), &seller);
        seller.system_prompt = Some("You are a sales rep.".into());
        let s_v2 = compute_draft_signature(&lead, Some("msg-1"), &seller);
        assert_ne!(s_v1, s_v2);
    }

    #[test]
    fn signature_changes_when_draft_template_changes() {
        let lead = lead_fixture();
        let mut seller = seller_fixture();
        seller.draft_template = Some("template v1".into());
        let s1 = compute_draft_signature(&lead, Some("msg-1"), &seller);
        seller.draft_template = Some("template v2".into());
        let s2 = compute_draft_signature(&lead, Some("msg-1"), &seller);
        assert_ne!(s1, s2);
    }

    #[test]
    fn signature_changes_when_model_changes() {
        let lead = lead_fixture();
        let mut seller = seller_fixture();
        seller.model_provider = Some("minimax".into());
        seller.model_id = Some("m2.5".into());
        let s_minimax = compute_draft_signature(&lead, Some("msg-1"), &seller);
        seller.model_provider = Some("anthropic".into());
        seller.model_id = Some("claude-opus-4-7".into());
        let s_claude = compute_draft_signature(&lead, Some("msg-1"), &seller);
        assert_ne!(s_minimax, s_claude);
    }

    #[test]
    fn last_inbound_message_id_picks_most_recent_inbound() {
        let inb = |id: &str, at: i64| ThreadMessage {
            id: id.into(),
            direction: MessageDirection::Inbound,
            from_label: "Juan".into(),
            body: "x".into(),
            at_ms: at,
            draft_status: None,
            subject: None,
        };
        let outb = |id: &str, at: i64| ThreadMessage {
            id: id.into(),
            direction: MessageDirection::Outbound,
            from_label: "Pedro".into(),
            body: "x".into(),
            at_ms: at,
            draft_status: None,
            subject: None,
        };
        let history = vec![
            inb("m1", 100),
            outb("m2", 200),
            inb("m3", 300),
            outb("m4", 400),
        ];
        assert_eq!(last_inbound_message_id(&history), Some("m3"));
        let empty: Vec<ThreadMessage> = vec![];
        assert_eq!(last_inbound_message_id(&empty), None);
    }
}

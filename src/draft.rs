//! M15.21 slice 4 — pluggable draft generation.
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

use nexo_microapp_sdk::templating::handlebars::{
    render_handlebars, HandlebarsRenderError,
};
use nexo_tool_meta::marketing::{Lead, Seller};

use crate::lead::ThreadMessage;

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
    };
    let inbound = crate::lead::ThreadMessage {
        id: "preview-msg".into(),
        direction: crate::lead::MessageDirection::Inbound,
        from_label: "Preview".into(),
        body: "Preview body".into(),
        at_ms: Utc::now().timestamp_millis(),
        draft_status: None,
    };
    let ctx = DraftContext {
        lead,
        seller,
        last_inbound: Some(inbound),
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
        let body = self.template.load_full();
        let rendered = render_handlebars(&body, &context)?;
        let trimmed = rendered.trim();
        if trimmed.is_empty() {
            return Err(DraftGenError::Empty);
        }
        Ok(rendered)
    }
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
        }
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
            operator_hint: None,
        };
        let err = gen.generate(&ctx).await.unwrap_err();
        assert!(matches!(
            err,
            DraftGenError::TemplateParse(_) | DraftGenError::TemplateRender(_)
        ));
    }
}

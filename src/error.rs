//! Typed error surface for the marketing extension.
//!
//! Discriminated union — every fallible operation returns
//! `Result<_, MarketingError>` so callers always switch on a
//! variant. Mirrors the convention used by
//! `crates/plugins/email/src/plugin.rs` + the SDK's
//! `VoiceError`.

use thiserror::Error;

use crate::tenant::TenantId;
use nexo_tool_meta::marketing::{LeadId, LeadState};

/// Public error type for every operation in the extension.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum MarketingError {
    /// SQLite operation failed (open, DDL, INSERT, SELECT, …).
    #[error("sqlite: {0}")]
    Sqlite(#[from] sqlx::Error),

    /// Caller asked for a state transition the lead state
    /// machine doesn't accept (e.g. cold → qualified directly).
    #[error("invalid transition {from:?} → {to:?} for lead {lead_id:?}")]
    InvalidTransition {
        /// Lead the operator was trying to advance.
        lead_id: LeadId,
        /// Current state on disk.
        from: LeadState,
        /// State the caller asked us to move to.
        to: LeadState,
    },

    /// Bearer token was valid but the resolved tenant id does
    /// not match what the caller passed (typically a microapp
    /// bug or a deliberate cross-tenant probe).
    #[error("tenant_unauthorised: {0:?}")]
    TenantUnauthorised(TenantId),

    /// Identity classifier rejected its input or returned an
    /// unrecoverable error from a downstream source.
    #[error("classifier: {0}")]
    Classifier(String),

    /// Web scraper failed (HTTP error, robots.txt blocked,
    /// fetch budget exhausted, …).
    #[error("scraper: {0}")]
    Scraper(String),

    /// LLM call failed (timeout, budget exceeded, provider
    /// error). Caller usually falls back to a deterministic
    /// branch.
    #[error("llm: {0}")]
    Llm(String),

    /// `nexo-compliance-primitives::pre_outbound_check`
    /// blocked the outbound (opt-out, anti-loop, PII redaction,
    /// rate-limit). Reason carries the operator-facing message.
    #[error("compliance blocked: {0}")]
    Compliance(String),

    /// YAML config failed schema validation. Operator hand-
    /// edited the file or the schema bumped without migration.
    #[error("config: {0}")]
    Config(String),

    /// Broker publish or subscribe failed.
    #[error("broker: {0}")]
    Broker(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexo_tool_meta::marketing::{LeadId, LeadState};

    #[test]
    fn error_message_includes_lead_id_for_invalid_transition() {
        let e = MarketingError::InvalidTransition {
            lead_id: LeadId("lead-001".into()),
            from: LeadState::Cold,
            to: LeadState::Qualified,
        };
        let msg = e.to_string();
        assert!(msg.contains("lead-001"));
        assert!(msg.contains("Cold"));
        assert!(msg.contains("Qualified"));
    }

    #[test]
    fn sqlite_error_propagates_via_from() {
        // smoke: From<sqlx::Error> wired
        let underlying = sqlx::Error::RowNotFound;
        let e: MarketingError = underlying.into();
        assert!(matches!(e, MarketingError::Sqlite(_)));
    }

    #[test]
    fn compliance_message_carries_reason() {
        let e = MarketingError::Compliance("opted_out".into());
        assert_eq!(e.to_string(), "compliance blocked: opted_out");
    }

    #[test]
    fn tenant_unauthorised_message() {
        let e = MarketingError::TenantUnauthorised(TenantId::new("acme").unwrap());
        let msg = e.to_string();
        assert!(msg.contains("acme"));
    }
}

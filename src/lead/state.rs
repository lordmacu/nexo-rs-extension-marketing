//! Lead state machine — pure validation logic, no IO.
//!
//! ```text
//!         cliente_writes
//! cold ─────────────────────────► engaged
//!                                     │
//!                                     │ accepts_meeting
//!                                     ▼
//!                              meeting_scheduled
//!                                     │
//!                                     │ confirmed (operator/vendor)
//!                                     ▼
//!                                 qualified
//!
//! Anywhere except `lost` ──────► lost  (compliance, 3+ followups, manual)
//! ```

use nexo_tool_meta::marketing::{LeadId, LeadState};

use crate::error::MarketingError;

/// Set of legal target states for a given current state. Pure
/// data — no allocation, no IO. Tests cover every legal +
/// every illegal pair.
#[derive(Debug, Clone, Copy)]
pub struct LegalTransitions;

impl LegalTransitions {
    /// True iff `from → to` is a legal transition.
    pub fn allows(from: LeadState, to: LeadState) -> bool {
        if from == to {
            // No-op transitions never allowed — caller would
            // emit an event for nothing. If the operator wants
            // to "refresh" a lead, do it through a
            // domain-specific action, not a transition.
            return false;
        }
        match (from, to) {
            // Forward path
            (LeadState::Cold, LeadState::Engaged) => true,
            (LeadState::Engaged, LeadState::MeetingScheduled) => true,
            (LeadState::MeetingScheduled, LeadState::Qualified) => true,

            // Any non-terminal state can drop to `lost` (compliance,
            // exhausted followups, operator manual). `lost` itself
            // is terminal — never re-opens; the operator creates a
            // fresh lead instead so the audit trail stays clean.
            (LeadState::Cold, LeadState::Lost) => true,
            (LeadState::Engaged, LeadState::Lost) => true,
            (LeadState::MeetingScheduled, LeadState::Lost) => true,
            (LeadState::Qualified, LeadState::Lost) => true,

            // Backward path: meeting fell through, drop to engaged.
            // Nothing else moves backward — all forward transitions
            // are one-way.
            (LeadState::MeetingScheduled, LeadState::Engaged) => true,

            // Everything else illegal — explicit catch-all so a
            // future enum variant lights up an obvious gap here
            // instead of silently returning `true`.
            _ => false,
        }
    }
}

/// Validate a transition. Returns `Ok(())` on legal, the typed
/// error on illegal so callers can propagate it through the
/// extension's `Result<_, MarketingError>` chain.
pub fn validate_transition(
    lead_id: &LeadId,
    from: LeadState,
    to: LeadState,
) -> Result<(), MarketingError> {
    if LegalTransitions::allows(from, to) {
        Ok(())
    } else {
        Err(MarketingError::InvalidTransition {
            lead_id: lead_id.clone(),
            from,
            to,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lid() -> LeadId {
        LeadId("lead-001".into())
    }

    // ── Legal transitions ─────────────────────────────────────

    #[test]
    fn cold_to_engaged_legal() {
        validate_transition(&lid(), LeadState::Cold, LeadState::Engaged).unwrap();
    }

    #[test]
    fn engaged_to_meeting_legal() {
        validate_transition(&lid(), LeadState::Engaged, LeadState::MeetingScheduled)
            .unwrap();
    }

    #[test]
    fn meeting_to_qualified_legal() {
        validate_transition(
            &lid(),
            LeadState::MeetingScheduled,
            LeadState::Qualified,
        )
        .unwrap();
    }

    #[test]
    fn meeting_drops_back_to_engaged() {
        // Meeting fell through (cancellation, reschedule miss).
        // Single legal backward transition in v1.
        validate_transition(
            &lid(),
            LeadState::MeetingScheduled,
            LeadState::Engaged,
        )
        .unwrap();
    }

    #[test]
    fn any_non_terminal_to_lost_legal() {
        for from in [
            LeadState::Cold,
            LeadState::Engaged,
            LeadState::MeetingScheduled,
            LeadState::Qualified,
        ] {
            assert!(
                LegalTransitions::allows(from, LeadState::Lost),
                "{from:?} → Lost should be legal"
            );
        }
    }

    // ── Illegal transitions ───────────────────────────────────

    #[test]
    fn cold_to_meeting_illegal_must_pass_through_engaged() {
        let err = validate_transition(
            &lid(),
            LeadState::Cold,
            LeadState::MeetingScheduled,
        )
        .unwrap_err();
        assert!(matches!(err, MarketingError::InvalidTransition { .. }));
    }

    #[test]
    fn cold_to_qualified_illegal_two_skips() {
        let err = validate_transition(&lid(), LeadState::Cold, LeadState::Qualified)
            .unwrap_err();
        assert!(matches!(err, MarketingError::InvalidTransition { .. }));
    }

    #[test]
    fn engaged_to_qualified_illegal_must_meet_first() {
        let err = validate_transition(
            &lid(),
            LeadState::Engaged,
            LeadState::Qualified,
        )
        .unwrap_err();
        assert!(matches!(err, MarketingError::InvalidTransition { .. }));
    }

    #[test]
    fn lost_is_terminal_no_reopen() {
        for to in [
            LeadState::Cold,
            LeadState::Engaged,
            LeadState::MeetingScheduled,
            LeadState::Qualified,
        ] {
            let err = validate_transition(&lid(), LeadState::Lost, to).unwrap_err();
            assert!(matches!(err, MarketingError::InvalidTransition { .. }));
        }
    }

    #[test]
    fn qualified_no_reverse_to_engaged() {
        // Once qualified, the lead is closed/won. Reverting to
        // engaged means starting over — operator creates a new
        // lead instead.
        let err = validate_transition(
            &lid(),
            LeadState::Qualified,
            LeadState::Engaged,
        )
        .unwrap_err();
        assert!(matches!(err, MarketingError::InvalidTransition { .. }));
    }

    #[test]
    fn engaged_to_cold_illegal_no_unwarming() {
        let err = validate_transition(&lid(), LeadState::Engaged, LeadState::Cold)
            .unwrap_err();
        assert!(matches!(err, MarketingError::InvalidTransition { .. }));
    }

    #[test]
    fn no_op_transition_rejected() {
        for s in [
            LeadState::Cold,
            LeadState::Engaged,
            LeadState::MeetingScheduled,
            LeadState::Qualified,
            LeadState::Lost,
        ] {
            let err = validate_transition(&lid(), s, s).unwrap_err();
            assert!(matches!(err, MarketingError::InvalidTransition { .. }));
        }
    }

    #[test]
    fn invalid_transition_carries_lead_id_and_states() {
        let err = validate_transition(
            &LeadId("lead-xyz".into()),
            LeadState::Cold,
            LeadState::Qualified,
        )
        .unwrap_err();
        if let MarketingError::InvalidTransition { lead_id, from, to } = err {
            assert_eq!(lead_id.0, "lead-xyz");
            assert_eq!(from, LeadState::Cold);
            assert_eq!(to, LeadState::Qualified);
        } else {
            panic!("expected InvalidTransition");
        }
    }
}

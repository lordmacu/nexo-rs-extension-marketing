//! Per-tenant topic guardrail handle (M15.23.d).
//!
//! Wraps the SDK's `GuardrailSet` in an `arc_swap`-backed
//! handle so `PUT /config/topic_guardrails` can hot-swap the
//! compiled set under the broker hop without a process
//! restart. Mirrors the `RouterHandle` + `SellerLookup`
//! pattern used by every other live-reloaded YAML in the
//! extension.

use std::sync::Arc;

use arc_swap::ArcSwap;
use nexo_microapp_sdk::guardrails::{GuardrailLoadError, GuardrailRule, GuardrailSet};

/// Lock-free atomic handle. Readers (`load_full()`) get a
/// snapshot Arc immune to concurrent stores; writers (admin
/// `PUT`) build a fresh `GuardrailSet` from disk and call
/// `handle.store(new_arc)`.
pub type GuardrailHandle = Arc<ArcSwap<GuardrailSet>>;

/// Build a fresh handle from a compiled set. Call once at
/// boot with the operator's `topic_guardrails.yaml`.
pub fn handle_from_set(set: GuardrailSet) -> GuardrailHandle {
    Arc::new(ArcSwap::from_pointee(set))
}

/// Build a handle starting from an empty set. Useful when
/// the operator hasn't authored any guardrails yet — every
/// scan returns no matches.
pub fn empty_handle() -> GuardrailHandle {
    handle_from_set(GuardrailSet::empty())
}

/// Compile + wrap. Convenience for boot path that just
/// loaded the YAML.
pub fn handle_from_rules(
    rules: Vec<GuardrailRule>,
) -> Result<GuardrailHandle, GuardrailLoadError> {
    let set = GuardrailSet::build(rules)?;
    Ok(handle_from_set(set))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexo_microapp_sdk::guardrails::GuardrailAction;

    fn rule(id: &str) -> GuardrailRule {
        GuardrailRule {
            id: id.into(),
            name: id.into(),
            patterns: vec!["pricing".into()],
            action: GuardrailAction::ForceApproval,
        }
    }

    #[test]
    fn empty_handle_scans_to_no_matches() {
        let h = empty_handle();
        let snap = h.load_full();
        assert!(snap.scan("pricing inquiry").is_empty());
    }

    #[test]
    fn handle_from_rules_compiles() {
        let h = handle_from_rules(vec![rule("pricing")]).unwrap();
        let snap = h.load_full();
        assert_eq!(snap.rule_count(), 1);
        assert_eq!(snap.scan("pricing inquiry").len(), 1);
    }

    #[test]
    fn handle_swap_picks_up_new_rules() {
        let h = empty_handle();
        // Initial scan returns no hits.
        assert!(h.load_full().scan("pricing inquiry").is_empty());
        // Swap in a populated set; subsequent reads see it.
        let fresh = GuardrailSet::build(vec![rule("pricing")]).unwrap();
        h.store(Arc::new(fresh));
        assert_eq!(h.load_full().scan("pricing inquiry").len(), 1);
    }

    #[test]
    fn handle_from_rules_propagates_invalid_regex() {
        let bad = GuardrailRule {
            id: "x".into(),
            name: "x".into(),
            patterns: vec!["[unclosed".into()],
            action: GuardrailAction::Block,
        };
        let r = handle_from_rules(vec![bad]);
        assert!(matches!(r, Err(GuardrailLoadError::InvalidPattern { .. })));
    }
}

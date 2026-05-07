//! Per-tenant routing dispatcher.
//!
//! Thin wrapper over the SDK's `routing::Dispatcher` that:
//!   1. Loads the per-tenant `RuleSet` (file path encodes
//!      tenant scope; YAML hot-swappable).
//!   2. Builds a `MatchContext` from the parsed inbound +
//!      enrichment results + score.
//!   3. Calls the SDK dispatcher.
//!   4. Walks the round-robin cursor when the matched target
//!      is `RoundRobin`.

use std::path::Path;

use nexo_microapp_sdk::routing::{
    load_rule_set_from_str, AssignTarget, Dispatcher, DomainKind, MatchContext, RuleSet,
    RoutingDecision, RoutingError, TenantIdRef, VendedorId,
};

use crate::error::MarketingError;
use crate::tenant::TenantId;

/// Loads `<state_root>/marketing/<tenant_id>/rules.yaml` and
/// parses into a `RuleSet`. Missing file is NOT an error —
/// returns an empty rule set with `default_target = Drop` so
/// the operator gets a sensible "configure rules first" path
/// instead of panic.
pub fn load_rule_set(
    state_root: impl AsRef<Path>,
    tenant_id: &TenantId,
) -> Result<RuleSet, MarketingError> {
    let path = tenant_id.state_dir(state_root.as_ref()).join("rules.yaml");
    if !path.exists() {
        return Ok(empty_rule_set(tenant_id));
    }
    let yaml = std::fs::read_to_string(&path)
        .map_err(|e| MarketingError::Config(format!("read {}: {e}", path.display())))?;
    load_rule_set_from_str(TenantIdRef(tenant_id.as_str().to_string()), &yaml)
        .map_err(|e| MarketingError::Config(format!("parse rules.yaml: {e}")))
}

fn empty_rule_set(tenant_id: &TenantId) -> RuleSet {
    RuleSet {
        tenant_id: TenantIdRef(tenant_id.as_str().to_string()),
        version: 0,
        rules: Vec::new(),
        // Default Drop until operator authors at least one
        // rule. Conservative — we'd rather drop than auto-
        // assign to a wrong vendedor.
        default_target: AssignTarget::Drop,
    }
}

/// Per-call routing inputs the dispatcher reads. Owned strings
/// because the SDK MatchContext borrows; we materialise here so
/// the extension's outer call sites can pass owned values
/// without lifetime gymnastics.
#[derive(Debug, Clone, Default)]
pub struct RouteInputs {
    pub sender_email: Option<String>,
    pub sender_domain_kind: Option<DomainKind>,
    pub company_industry: Option<String>,
    pub person_tags: Vec<String>,
    pub score: Option<u8>,
    pub body: Option<String>,
    pub subject: Option<String>,
}

/// Routing dispatcher scoped to a tenant. Holds the rule set
/// + a round-robin cursor (atomic, thread-safe via simple
/// AtomicUsize wrapped in Arc — pure in-memory; cron tick
/// rebuilds on every YAML edit).
pub struct LeadRouter {
    tenant_id: TenantId,
    rule_set: RuleSet,
    rr_cursor: std::sync::atomic::AtomicUsize,
}

impl LeadRouter {
    pub fn new(tenant_id: TenantId, rule_set: RuleSet) -> Self {
        Self {
            tenant_id,
            rule_set,
            rr_cursor: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    pub fn rule_set(&self) -> &RuleSet {
        &self.rule_set
    }

    /// Run the dispatcher. Returns a flat `RouteOutcome` so the
    /// caller doesn't have to peek inside the SDK's tagged
    /// union.
    pub fn route(&self, input: &RouteInputs) -> Result<RouteOutcome, MarketingError> {
        let person_tag_refs: Vec<&str> = input.person_tags.iter().map(|s| s.as_str()).collect();
        let ctx = MatchContext {
            sender_email: input.sender_email.as_deref(),
            sender_domain_kind: input.sender_domain_kind,
            company_industry: input.company_industry.as_deref(),
            person_tags: &person_tag_refs,
            score: input.score,
            body: input.body.as_deref(),
            subject: input.subject.as_deref(),
        };
        let decision: RoutingDecision = Dispatcher::new(&self.rule_set)
            .dispatch(&self.rule_set.tenant_id, &ctx)
            .map_err(|e| match e {
                RoutingError::TenantMismatch { expected, got } => MarketingError::Config(
                    format!("tenant mismatch in router: expected {expected:?}, got {got:?}"),
                ),
            })?;
        Ok(self.flatten(decision))
    }

    fn flatten(&self, d: RoutingDecision) -> RouteOutcome {
        match d.target {
            AssignTarget::Vendedor { id } => RouteOutcome::Vendedor {
                vendedor_id: id,
                matched_rule_id: d.matched_rule_id,
                why: d.why,
            },
            AssignTarget::Drop => RouteOutcome::Drop {
                matched_rule_id: d.matched_rule_id,
                why: d.why,
            },
            AssignTarget::RoundRobin { pool } => {
                if pool.is_empty() {
                    return RouteOutcome::NoTarget {
                        matched_rule_id: d.matched_rule_id,
                        why: vec!["round-robin pool empty".into()],
                    };
                }
                let cursor = self
                    .rr_cursor
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let pick = &pool[cursor % pool.len()];
                RouteOutcome::Vendedor {
                    vendedor_id: pick.clone(),
                    matched_rule_id: d.matched_rule_id,
                    why: d.why,
                }
            }
        }
    }
}

/// What the extension does with the inbound after routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Hand off to a specific vendedor — single match or
    /// round-robin pick.
    Vendedor {
        vendedor_id: VendedorId,
        matched_rule_id: Option<String>,
        why: Vec<String>,
    },
    /// Disposable / spam — drop without notifying anyone.
    Drop {
        matched_rule_id: Option<String>,
        why: Vec<String>,
    },
    /// Rule resolved to a round-robin pool but the pool was
    /// empty (operator misconfigured). Caller falls back to
    /// the default rule's target or surfaces the error.
    NoTarget {
        matched_rule_id: Option<String>,
        why: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexo_microapp_sdk::routing::{
        RoutingRule, RulePredicate, TenantIdRef, VendedorId,
    };

    fn rs(t: &str, rules: Vec<RoutingRule>, default: AssignTarget) -> RuleSet {
        RuleSet {
            tenant_id: TenantIdRef(t.into()),
            version: 1,
            rules,
            default_target: default,
        }
    }
    fn vendedor(id: &str) -> VendedorId {
        VendedorId(id.into())
    }
    fn rule(id: &str, conds: Vec<RulePredicate>, target: AssignTarget) -> RoutingRule {
        RoutingRule {
            id: id.into(),
            name: id.into(),
            conditions: conds,
            assigns_to: target,
            followup_profile: "default".into(),
            active: true,
        }
    }

    #[test]
    fn corporate_high_score_routes_to_pedro() {
        let rs = rs(
            "acme",
            vec![rule(
                "warm-corp",
                vec![
                    RulePredicate::SenderDomainKind { value: DomainKind::Corporate },
                    RulePredicate::ScoreGte { score: 70 },
                ],
                AssignTarget::Vendedor { id: vendedor("pedro") },
            )],
            AssignTarget::Drop,
        );
        let r = LeadRouter::new(TenantId::new("acme").unwrap(), rs);
        let out = r
            .route(&RouteInputs {
                sender_email: Some("juan@acme.com".into()),
                sender_domain_kind: Some(DomainKind::Corporate),
                score: Some(75),
                ..Default::default()
            })
            .unwrap();
        match out {
            RouteOutcome::Vendedor {
                vendedor_id,
                matched_rule_id,
                ..
            } => {
                assert_eq!(vendedor_id, vendedor("pedro"));
                assert_eq!(matched_rule_id.as_deref(), Some("warm-corp"));
            }
            other => panic!("expected Vendedor, got {other:?}"),
        }
    }

    #[test]
    fn disposable_routes_to_drop_via_default() {
        let rs = rs(
            "acme",
            vec![rule(
                "drop-disposable",
                vec![RulePredicate::SenderDomainKind {
                    value: DomainKind::Disposable,
                }],
                AssignTarget::Drop,
            )],
            AssignTarget::Vendedor { id: vendedor("luis") },
        );
        let r = LeadRouter::new(TenantId::new("acme").unwrap(), rs);
        let out = r
            .route(&RouteInputs {
                sender_email: Some("spam@mailinator.com".into()),
                sender_domain_kind: Some(DomainKind::Disposable),
                ..Default::default()
            })
            .unwrap();
        assert!(matches!(out, RouteOutcome::Drop { .. }));
    }

    #[test]
    fn round_robin_distributes_across_pool() {
        let rs = rs(
            "acme",
            vec![],
            AssignTarget::RoundRobin {
                pool: vec![vendedor("a"), vendedor("b"), vendedor("c")],
            },
        );
        let r = LeadRouter::new(TenantId::new("acme").unwrap(), rs);
        let mut picks = Vec::new();
        for _ in 0..6 {
            let out = r.route(&RouteInputs::default()).unwrap();
            if let RouteOutcome::Vendedor { vendedor_id, .. } = out {
                picks.push(vendedor_id.0);
            }
        }
        // Round-robin wraps; over 6 calls each vendedor gets 2.
        let count_a = picks.iter().filter(|p| *p == "a").count();
        let count_b = picks.iter().filter(|p| *p == "b").count();
        let count_c = picks.iter().filter(|p| *p == "c").count();
        assert_eq!(count_a, 2);
        assert_eq!(count_b, 2);
        assert_eq!(count_c, 2);
    }

    #[test]
    fn empty_round_robin_pool_returns_no_target() {
        let rs = rs(
            "acme",
            vec![],
            AssignTarget::RoundRobin { pool: vec![] },
        );
        let r = LeadRouter::new(TenantId::new("acme").unwrap(), rs);
        let out = r.route(&RouteInputs::default()).unwrap();
        assert!(matches!(out, RouteOutcome::NoTarget { .. }));
    }

    #[test]
    fn default_target_used_when_no_rule_matches() {
        let rs = rs(
            "acme",
            vec![rule(
                "vip",
                vec![RulePredicate::PersonHasTag { tag: "vip".into() }],
                AssignTarget::Vendedor { id: vendedor("ana") },
            )],
            AssignTarget::Vendedor { id: vendedor("luis") },
        );
        let r = LeadRouter::new(TenantId::new("acme").unwrap(), rs);
        let out = r
            .route(&RouteInputs {
                sender_email: Some("juan@acme.com".into()),
                sender_domain_kind: Some(DomainKind::Corporate),
                person_tags: vec![], // no vip tag
                ..Default::default()
            })
            .unwrap();
        match out {
            RouteOutcome::Vendedor {
                vendedor_id,
                matched_rule_id,
                ..
            } => {
                assert_eq!(vendedor_id, vendedor("luis"));
                assert!(matched_rule_id.is_none(), "default target carries no rule id");
            }
            other => panic!("expected default Vendedor, got {other:?}"),
        }
    }

    #[test]
    fn missing_rules_yaml_loads_empty_drop_default() {
        let tmp = tempfile::tempdir().unwrap();
        let tenant = TenantId::new("acme").unwrap();
        let rs = load_rule_set(tmp.path(), &tenant).unwrap();
        assert!(rs.rules.is_empty());
        assert!(matches!(rs.default_target, AssignTarget::Drop));
    }

    #[test]
    fn loads_yaml_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let tenant = TenantId::new("acme").unwrap();
        let dir = tenant.state_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rules.yaml"),
            r#"
version: 1
default_target:
  kind: drop
rules:
  - id: vip
    name: VIP
    active: true
    followup_profile: vip
    conditions:
      - kind: person_has_tag
        tag: vip
    assigns_to:
      kind: vendedor
      id: "ana"
"#,
        )
        .unwrap();
        let rs = load_rule_set(tmp.path(), &tenant).unwrap();
        assert_eq!(rs.version, 1);
        assert_eq!(rs.rules.len(), 1);
        assert_eq!(rs.rules[0].id, "vip");
    }
}

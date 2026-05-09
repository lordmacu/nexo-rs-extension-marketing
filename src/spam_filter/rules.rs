//! `ResolvedRules` — defaults ⊕ tenant overrides, ready to feed
//! the classifier on the hot path. `RulesCache` keeps an Arc per
//! tenant so the broker hop reads without ever touching SQLite
//! after warm-up; admin writes invalidate the cached entry.
//!
//! The defaults (keyword + role lists) ship hardcoded; tenant
//! `KeywordBlock` rules extend the keyword list, `KeywordAllow`
//! rules suppress hits, `DomainBlock` / `SenderBlock` short-
//! circuit to a drop verdict, the `*_Allow` peers short-circuit
//! to `Human` regardless of every other signal.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::RwLock;

use super::defaults::{
    Strictness, ThresholdSet, BALANCED_THRESHOLDS, DEFAULT_PROMO_KEYWORDS,
    DEFAULT_ROLE_LOCAL_PARTS,
};
use super::store::{RuleKind, SpamFilterRule, SpamFilterStore, StoreError};

/// Per-tenant resolved rules — what the classifier actually
/// reads. Cheap to clone (string sets behind `Arc`s).
#[derive(Debug, Clone, Default)]
pub struct ResolvedRules {
    pub strictness: Strictness,
    pub thresholds: ThresholdSet,
    /// Allow lists short-circuit to `Human`. Lowercase.
    pub allow_domains: HashSet<String>,
    pub allow_senders: HashSet<String>,
    pub allow_keywords: HashSet<String>,
    /// Block lists short-circuit to `Promo`. Lowercase.
    pub block_domains: HashSet<String>,
    pub block_senders: HashSet<String>,
    /// Built-in keyword vocabulary plus tenant block additions.
    /// Ordered for deterministic hit-counting in tests.
    pub keywords: Vec<String>,
    pub role_local_parts: Vec<String>,
}

impl ResolvedRules {
    /// Default-only resolution — no tenant rules. Used by the
    /// classifier in tests + as the fallback when the cache miss
    /// path can't reach SQLite (degraded mode).
    pub fn defaults_only() -> Self {
        let keywords: Vec<String> = DEFAULT_PROMO_KEYWORDS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let role_local_parts: Vec<String> = DEFAULT_ROLE_LOCAL_PARTS
            .iter()
            .map(|s| s.to_string())
            .collect();
        Self {
            strictness: Strictness::Balanced,
            thresholds: BALANCED_THRESHOLDS.clone(),
            allow_domains: HashSet::new(),
            allow_senders: HashSet::new(),
            allow_keywords: HashSet::new(),
            block_domains: HashSet::new(),
            block_senders: HashSet::new(),
            keywords,
            role_local_parts,
        }
    }

    /// Resolve from the persisted `(config, rules)` tuple. The
    /// active threshold set follows `config.strictness` —
    /// `Custom` uses the per-tenant thresholds, the three fixed
    /// presets snapshot to the constant tables (the per-tenant
    /// thresholds row is still kept around so the UI can show
    /// "saved custom" alongside the preset radios).
    pub fn from_persisted(
        strictness: Strictness,
        custom_thresholds: ThresholdSet,
        rules: &[SpamFilterRule],
    ) -> Self {
        let thresholds = match strictness {
            Strictness::Custom => custom_thresholds,
            preset => ThresholdSet::for_preset(preset),
        };

        let mut allow_domains = HashSet::new();
        let mut allow_senders = HashSet::new();
        let mut allow_keywords: HashSet<String> = HashSet::new();
        let mut block_domains = HashSet::new();
        let mut block_senders = HashSet::new();
        let mut tenant_keyword_blocks: Vec<String> = Vec::new();

        for r in rules {
            if !r.enabled {
                continue;
            }
            let v = r.value.clone();
            match r.kind {
                RuleKind::DomainBlock => {
                    block_domains.insert(v);
                }
                RuleKind::DomainAllow => {
                    allow_domains.insert(v);
                }
                RuleKind::KeywordBlock => {
                    tenant_keyword_blocks.push(v);
                }
                RuleKind::KeywordAllow => {
                    allow_keywords.insert(v);
                }
                RuleKind::SenderBlock => {
                    block_senders.insert(v);
                }
                RuleKind::SenderAllow => {
                    allow_senders.insert(v);
                }
            }
        }

        // Compose final keyword vocabulary: defaults minus
        // allow-suppressions, plus tenant blocks.
        let mut keywords: Vec<String> = DEFAULT_PROMO_KEYWORDS
            .iter()
            .filter(|kw| !allow_keywords.contains(**kw))
            .map(|s| s.to_string())
            .collect();
        for kw in tenant_keyword_blocks {
            if !keywords.iter().any(|existing| existing == &kw) {
                keywords.push(kw);
            }
        }

        let role_local_parts: Vec<String> = DEFAULT_ROLE_LOCAL_PARTS
            .iter()
            .map(|s| s.to_string())
            .collect();

        Self {
            strictness,
            thresholds,
            allow_domains,
            allow_senders,
            allow_keywords,
            block_domains,
            block_senders,
            keywords,
            role_local_parts,
        }
    }
}

/// Hot-path cache: tenant_id → resolved Arc. Admin writes call
/// `invalidate(tenant_id)` so the next `get_or_load` picks up
/// the fresh state.
#[derive(Clone)]
pub struct RulesCache {
    store: SpamFilterStore,
    inner: Arc<RwLock<HashMap<String, Arc<ResolvedRules>>>>,
}

impl RulesCache {
    pub fn new(store: SpamFilterStore) -> Self {
        Self {
            store,
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn store(&self) -> &SpamFilterStore {
        &self.store
    }

    /// Load (or reuse) the resolved rules for a tenant. Errors
    /// from the store are logged and the cache returns
    /// `defaults_only()` so the classifier still fires (we
    /// prefer best-effort filtering over dropping every inbound
    /// when SQLite hiccups).
    pub async fn get_or_load(&self, tenant_id: &str, now_ms: i64) -> Arc<ResolvedRules> {
        if let Some(hit) = self.inner.read().await.get(tenant_id).cloned() {
            return hit;
        }
        match self.load_from_store(tenant_id, now_ms).await {
            Ok(resolved) => {
                let arc = Arc::new(resolved);
                self.inner
                    .write()
                    .await
                    .insert(tenant_id.to_string(), arc.clone());
                arc
            }
            Err(e) => {
                tracing::warn!(
                    target: "marketing.spam_filter",
                    tenant_id,
                    error = %e,
                    "spam filter rules load failed; falling back to defaults",
                );
                Arc::new(ResolvedRules::defaults_only())
            }
        }
    }

    /// Drop the cached entry for a tenant. Called by admin
    /// endpoints after every write so the next inbound sees the
    /// freshest state.
    pub async fn invalidate(&self, tenant_id: &str) {
        self.inner.write().await.remove(tenant_id);
    }

    /// Drop every cached entry. Used in tests + on hot-reload of
    /// the entire config (rare; mostly an operator escape hatch).
    pub async fn clear(&self) {
        self.inner.write().await.clear();
    }

    async fn load_from_store(
        &self,
        tenant_id: &str,
        now_ms: i64,
    ) -> Result<ResolvedRules, StoreError> {
        let cfg = self.store.get_config(tenant_id, now_ms).await?;
        let rules = self.store.list_rules(tenant_id).await?;
        Ok(ResolvedRules::from_persisted(
            cfg.strictness,
            cfg.thresholds,
            &rules,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spam_filter::defaults::STRICT_THRESHOLDS;

    fn rule(kind: RuleKind, value: &str) -> SpamFilterRule {
        SpamFilterRule {
            id: 1,
            tenant_id: "t".into(),
            kind,
            value: value.into(),
            note: None,
            enabled: true,
            created_at_ms: 0,
        }
    }

    #[test]
    fn defaults_only_carries_built_in_lists() {
        let r = ResolvedRules::defaults_only();
        assert_eq!(r.strictness, Strictness::Balanced);
        assert!(r.keywords.iter().any(|k| k == "unsubscribe"));
        assert!(r.role_local_parts.iter().any(|k| k == "noreply"));
    }

    #[test]
    fn from_persisted_strict_uses_strict_thresholds() {
        let r = ResolvedRules::from_persisted(
            Strictness::Strict,
            BALANCED_THRESHOLDS.clone(), // ignored when not Custom
            &[],
        );
        assert_eq!(r.thresholds, STRICT_THRESHOLDS);
    }

    #[test]
    fn from_persisted_custom_uses_supplied_thresholds() {
        let mut custom = BALANCED_THRESHOLDS.clone();
        custom.multi_weak_threshold = 99;
        let r = ResolvedRules::from_persisted(Strictness::Custom, custom, &[]);
        assert_eq!(r.thresholds.multi_weak_threshold, 99);
    }

    #[test]
    fn keyword_allow_suppresses_default() {
        let rules = vec![rule(RuleKind::KeywordAllow, "click here")];
        let r = ResolvedRules::from_persisted(
            Strictness::Balanced,
            BALANCED_THRESHOLDS.clone(),
            &rules,
        );
        assert!(!r.keywords.iter().any(|k| k == "click here"));
        assert!(r.allow_keywords.contains("click here"));
    }

    #[test]
    fn keyword_block_extends_default_list() {
        let rules = vec![rule(RuleKind::KeywordBlock, "casino bono")];
        let r = ResolvedRules::from_persisted(
            Strictness::Balanced,
            BALANCED_THRESHOLDS.clone(),
            &rules,
        );
        assert!(r.keywords.iter().any(|k| k == "casino bono"));
    }

    #[test]
    fn disabled_rules_are_ignored() {
        let mut rules = vec![rule(RuleKind::DomainBlock, "spam.com")];
        rules[0].enabled = false;
        let r = ResolvedRules::from_persisted(
            Strictness::Balanced,
            BALANCED_THRESHOLDS.clone(),
            &rules,
        );
        assert!(r.block_domains.is_empty());
    }

    #[test]
    fn rules_partition_by_kind() {
        let rules = vec![
            rule(RuleKind::DomainBlock, "spam.com"),
            rule(RuleKind::DomainAllow, "trusted.com"),
            rule(RuleKind::SenderBlock, "ads@bigcorp.com"),
            rule(RuleKind::SenderAllow, "vip@x.com"),
        ];
        let r = ResolvedRules::from_persisted(
            Strictness::Balanced,
            BALANCED_THRESHOLDS.clone(),
            &rules,
        );
        assert!(r.block_domains.contains("spam.com"));
        assert!(r.allow_domains.contains("trusted.com"));
        assert!(r.block_senders.contains("ads@bigcorp.com"));
        assert!(r.allow_senders.contains("vip@x.com"));
    }
}

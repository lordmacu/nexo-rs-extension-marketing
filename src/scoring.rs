//! Marketing-specific scorer composition (M15.23.f).
//!
//! **F29 sweep:** marketing-specific by design. Generic
//! `Scorer` / `HeuristicScorer` / `Score` types live in
//! `nexo_microapp_sdk::scoring`; this module composes the
//! 5 marketing-default rules + the `LeadCtx` projection.
//!
//! Wraps the SDK's generic `HeuristicScorer` with the lead-
//! shaped rules the marketing extension cares about today.
//! Pure logic + no async — the broker hop runs the scorer
//! synchronously inside its lead-create branch.
//!
//! ## Inputs
//!
//! [`LeadCtx`] is the minimal projection of the data the
//! broker hop has at create time: domain kind, body length,
//! signature role hint (when the resolver inferred one), and
//! the operator-supplied display name. Reply latency +
//! intent label aren't available yet on first inbound (no
//! prior outbound to compare against, no LLM intent stage
//! wired) — those rules light up later when the M22 draft
//! pipeline + M18 intent classifier land.
//!
//! ## Why heuristic v1
//!
//! Five rules cover the obvious operator intuition:
//!
//! - Corporate domain ⇒ +20 (most leads worth auto-routing)
//! - Personal domain ⇒ -5 (lower priority, but not blocked)
//! - Substantive body (≥ 30 words) ⇒ +10
//! - Brief body (< 5 words) ⇒ -10 (likely auto-reply)
//! - Sender display name carries a senior role hint ⇒ +15

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use thiserror::Error;
use tokio::sync::RwLock;

use nexo_microapp_sdk::enrichment::DomainKind;
use nexo_microapp_sdk::scoring::{HeuristicRule, HeuristicScorer, Score, Scorer};

use crate::broker::inbound::ParsedInbound;

/// Built-in default purchase-intent vocabulary (Spanish +
/// English). Operators extend / replace this list per tenant
/// via [`ScoringConfig.purchase_intent_keywords`]. Matching is
/// case-insensitive substring match against the
/// subject + body haystack.
pub const DEFAULT_PURCHASE_INTENT_KEYWORDS: &[&str] = &[
    // ── Spanish ─────────────────────────────────────────────
    "quiero comprar",
    "deseo comprar",
    "comprar tu",
    "comprar el",
    "comprar la",
    "cotizar",
    "cotización",
    "cotizacion",
    "cuánto cuesta",
    "cuanto cuesta",
    "cuánto vale",
    "cuanto vale",
    "precio",
    "presupuesto",
    "me interesa",
    "interesado en",
    "interesada en",
    "necesito comprar",
    "necesito contratar",
    "demo",
    "agendar",
    "agendar reunión",
    "agendar reunion",
    // ── English ─────────────────────────────────────────────
    "want to buy",
    "would like to buy",
    "interested in buying",
    "interested in",
    "looking to purchase",
    "looking to buy",
    "ready to buy",
    "get a quote",
    "request a quote",
    "pricing",
    "how much",
    "schedule a demo",
    "book a demo",
    "request a demo",
];

/// Built-in senior-role vocabulary (case-insensitive substring
/// over the sender's display name). Operators override per
/// tenant via [`ScoringConfig.senior_tokens`].
pub const DEFAULT_SENIOR_TOKENS: &[&str] = &[
    "ceo",
    "cto",
    "cfo",
    "coo",
    "vp ",
    "vice president",
    "director",
    "head of",
    "founder",
    "co-founder",
    "fundador",
    "fundadora",
    "gerente general",
    "presidente",
];

/// Tenant-tunable scoring weights + thresholds. Defaults match
/// the original hardcoded rules; operators override by writing
/// the row through `PUT /admin/scoring/config`.
///
/// Persisted as JSON inside `marketing_scoring_config` so adding
/// a new field is a forward-compatible change (older rows
/// deserialize with `Default` for the missing fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringConfig {
    /// Boost applied when the sender's domain classifies as
    /// `Corporate`.
    pub corporate_domain_boost: i32,
    /// Penalty applied when the sender's domain classifies as
    /// `Personal` (gmail / hotmail / …).
    pub personal_domain_penalty: i32,
    /// Word-count threshold above which the body is
    /// "substantive" (boost applies).
    pub substantive_body_min_words: u32,
    pub substantive_body_boost: i32,
    /// Word-count threshold below which the body is "brief"
    /// (penalty applies). Inclusive cap = `body_words <
    /// brief_body_max_words`.
    pub brief_body_max_words: u32,
    pub brief_body_penalty: i32,
    /// Purchase-intent keyword boost. Triggered when any
    /// `purchase_intent_keywords` substring matches the
    /// subject + body haystack (lowercase).
    pub purchase_intent_boost: i32,
    /// Lowercase substrings that flag a purchase / quote
    /// intent. Default = [`DEFAULT_PURCHASE_INTENT_KEYWORDS`].
    pub purchase_intent_keywords: Vec<String>,
    /// Boost applied when the sender's display name matches a
    /// senior-role token.
    pub senior_signature_boost: i32,
    /// Lowercase substrings the senior-role rule looks for in
    /// the display name. Default = [`DEFAULT_SENIOR_TOKENS`].
    pub senior_tokens: Vec<String>,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            corporate_domain_boost: 20,
            personal_domain_penalty: -5,
            substantive_body_min_words: 30,
            substantive_body_boost: 10,
            brief_body_max_words: 3,
            brief_body_penalty: -10,
            purchase_intent_boost: 25,
            purchase_intent_keywords: DEFAULT_PURCHASE_INTENT_KEYWORDS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            senior_signature_boost: 15,
            senior_tokens: DEFAULT_SENIOR_TOKENS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// Lead-shaped scorer input. Cheap to build off a
/// [`ParsedInbound`] (see [`LeadCtx::from_parsed`]).
#[derive(Debug, Clone)]
pub struct LeadCtx {
    /// Domain classifier verdict (`Corporate` / `Personal` /
    /// `Disposable` — `Disposable` short-circuits before
    /// scoring lands so it never gets here).
    pub domain_kind: DomainKind,
    /// Word count over the body excerpt (whitespace split).
    pub body_words: u32,
    /// Lower-cased sender display name when present. Used by
    /// the senior-signature rule.
    pub display_name_lower: Option<String>,
    /// Concatenated lowercase haystack of subject + body_excerpt
    /// for substring matches in semantic-intent rules. Built
    /// once at projection time so each rule iter doesn't
    /// re-allocate.
    pub haystack_lower: String,
}

impl LeadCtx {
    /// Build from the parsed inbound. Pure projection — no
    /// I/O.
    pub fn from_parsed(parsed: &ParsedInbound) -> Self {
        let body_words = parsed.body_excerpt.split_whitespace().count() as u32;
        let display_name_lower = parsed
            .from_display_name
            .as_ref()
            .map(|s| s.to_lowercase());
        let mut haystack_lower = String::with_capacity(
            parsed.subject.len() + parsed.body_excerpt.len() + 1,
        );
        haystack_lower.push_str(&parsed.subject.to_lowercase());
        haystack_lower.push(' ');
        haystack_lower.push_str(&parsed.body_excerpt.to_lowercase());
        Self {
            domain_kind: parsed.from_domain_kind,
            body_words,
            display_name_lower,
            haystack_lower,
        }
    }
}

/// Build the marketing heuristic scorer from a tenant-tunable
/// [`ScoringConfig`]. Six rules (corporate / personal /
/// substantive / brief / purchase_intent / senior_signature),
/// composable via [`Scorer`] dyn dispatch with future scorers
/// (sentiment, intent, BANT) when those land.
pub fn build_marketing_scorer_with_config(
    cfg: &ScoringConfig,
) -> HeuristicScorer<LeadCtx> {
    let mut s = HeuristicScorer::new();

    s.push(HeuristicRule::with_detail(
        "corporate_domain",
        cfg.corporate_domain_boost,
        "sender from a corporate domain",
        |c: &LeadCtx| c.domain_kind == DomainKind::Corporate,
    ));

    s.push(HeuristicRule::with_detail(
        "personal_domain",
        cfg.personal_domain_penalty,
        "sender from a personal domain (gmail, hotmail, …)",
        |c: &LeadCtx| c.domain_kind == DomainKind::Personal,
    ));

    let substantive_min = cfg.substantive_body_min_words;
    s.push(HeuristicRule::with_detail(
        "substantive_body",
        cfg.substantive_body_boost,
        "body length above the substantive threshold",
        move |c: &LeadCtx| c.body_words >= substantive_min,
    ));

    let brief_max = cfg.brief_body_max_words;
    s.push(HeuristicRule::with_detail(
        "brief_body",
        cfg.brief_body_penalty,
        "very brief body (typical auto-reply such as \"OK\" / \"Thanks\")",
        move |c: &LeadCtx| c.body_words < brief_max,
    ));

    // Strong positive signal — explicit purchase / quote
    // intent in subject or body wins regardless of length.
    // Catches "Quiero comprar" / "Cotizar" / "Cuánto cuesta"
    // / "Looking to buy" style high-intent leads the
    // word-count rule alone would mis-rank.
    let purchase_keywords = cfg.purchase_intent_keywords.clone();
    s.push(HeuristicRule::with_detail(
        "purchase_intent",
        cfg.purchase_intent_boost,
        "subject or body contains a purchase-intent keyword",
        move |c: &LeadCtx| {
            purchase_keywords
                .iter()
                .any(|kw| c.haystack_lower.contains(kw.as_str()))
        },
    ));

    let senior_tokens = cfg.senior_tokens.clone();
    s.push(HeuristicRule::with_detail(
        "senior_signature",
        cfg.senior_signature_boost,
        "display name mentions a senior role",
        move |c: &LeadCtx| {
            let Some(name) = c.display_name_lower.as_deref() else {
                return false;
            };
            senior_tokens.iter().any(|tok| name.contains(tok.as_str()))
        },
    ));

    s
}

/// Build the marketing-default heuristic scorer with the bundled
/// defaults. Equivalent to
/// `build_marketing_scorer_with_config(&ScoringConfig::default())`;
/// kept as a thin alias for callers that don't have a tenant
/// override loaded.
pub fn build_marketing_scorer() -> HeuristicScorer<LeadCtx> {
    build_marketing_scorer_with_config(&ScoringConfig::default())
}

/// Convenience wrapper: build the default scorer + apply it.
/// Tests invoke this directly without juggling the scorer
/// instance. Production code on the broker hop should call
/// [`score_lead_with_config`] so per-tenant overrides apply.
pub fn score_lead(parsed: &ParsedInbound) -> Score {
    build_marketing_scorer().score(&LeadCtx::from_parsed(parsed))
}

/// Build the scorer from the supplied config and run it against
/// the projected `LeadCtx`. The broker hop calls this with the
/// tenant's cached `ScoringConfig`.
pub fn score_lead_with_config(parsed: &ParsedInbound, cfg: &ScoringConfig) -> Score {
    build_marketing_scorer_with_config(cfg).score(&LeadCtx::from_parsed(parsed))
}

// ── Per-tenant persistence ───────────────────────────────────────

#[derive(Debug, Error)]
pub enum ScoringStoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] sqlx::Error),
    #[error("invalid config json: {0}")]
    InvalidJson(String),
}

const SCORING_MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS marketing_scoring_config (
    tenant_id     TEXT PRIMARY KEY,
    -- Whole ScoringConfig serialized as JSON so adding new
    -- fields is a forward-compatible change (older rows
    -- deserialize with defaults for the missing fields).
    config_json   TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
"#;

/// Run the migration. Idempotent — safe on every boot.
pub async fn migrate_scoring(pool: &SqlitePool) -> Result<(), ScoringStoreError> {
    sqlx::query(SCORING_MIGRATION_SQL).execute(pool).await?;
    Ok(())
}

/// SQLite-backed store for per-tenant `ScoringConfig`. Pairs
/// with [`ScoringConfigCache`] for the hot-path read used by
/// the broker hop.
#[derive(Clone)]
pub struct ScoringConfigStore {
    pool: SqlitePool,
}

impl ScoringConfigStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Read the per-tenant config. Returns the default config
    /// when the row doesn't exist yet — first-write happens on
    /// the first PUT.
    pub async fn get(&self, tenant_id: &str) -> Result<ScoringConfig, ScoringStoreError> {
        let row = sqlx::query(
            "SELECT config_json FROM marketing_scoring_config WHERE tenant_id = ?",
        )
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => {
                let json: String = r.try_get("config_json")?;
                serde_json::from_str(&json)
                    .map_err(|e| ScoringStoreError::InvalidJson(e.to_string()))
            }
            None => Ok(ScoringConfig::default()),
        }
    }

    /// Upsert the per-tenant config.
    pub async fn put(
        &self,
        tenant_id: &str,
        cfg: &ScoringConfig,
        now_ms: i64,
    ) -> Result<(), ScoringStoreError> {
        let json = serde_json::to_string(cfg)
            .map_err(|e| ScoringStoreError::InvalidJson(e.to_string()))?;
        sqlx::query(
            r#"INSERT INTO marketing_scoring_config (tenant_id, config_json, updated_at_ms)
               VALUES (?, ?, ?)
               ON CONFLICT(tenant_id) DO UPDATE SET
                   config_json   = excluded.config_json,
                   updated_at_ms = excluded.updated_at_ms"#,
        )
        .bind(tenant_id)
        .bind(json)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reset to defaults (delete the row).
    pub async fn reset(&self, tenant_id: &str) -> Result<bool, ScoringStoreError> {
        let res = sqlx::query("DELETE FROM marketing_scoring_config WHERE tenant_id = ?")
            .bind(tenant_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

/// Hot-path cache: tenant_id → resolved Arc<ScoringConfig>.
/// Admin writes call `invalidate(tenant_id)` so the next
/// broker hop picks up the fresh state.
#[derive(Clone)]
pub struct ScoringConfigCache {
    store: ScoringConfigStore,
    inner: Arc<RwLock<HashMap<String, Arc<ScoringConfig>>>>,
}

impl ScoringConfigCache {
    pub fn new(store: ScoringConfigStore) -> Self {
        Self {
            store,
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn store(&self) -> &ScoringConfigStore {
        &self.store
    }

    /// Load (or reuse) the resolved config for a tenant. Errors
    /// from the store are logged + cache returns defaults so the
    /// broker hop never fails the lead-create on a transient
    /// SQLite hiccup.
    pub async fn get_or_load(&self, tenant_id: &str) -> Arc<ScoringConfig> {
        if let Some(hit) = self.inner.read().await.get(tenant_id).cloned() {
            return hit;
        }
        match self.store.get(tenant_id).await {
            Ok(cfg) => {
                let arc = Arc::new(cfg);
                self.inner
                    .write()
                    .await
                    .insert(tenant_id.to_string(), arc.clone());
                arc
            }
            Err(e) => {
                tracing::warn!(
                    target: "marketing.scoring_cache",
                    tenant_id,
                    error = %e,
                    "scoring config load failed; falling back to defaults",
                );
                Arc::new(ScoringConfig::default())
            }
        }
    }

    pub async fn invalidate(&self, tenant_id: &str) {
        self.inner.write().await.remove(tenant_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_with(domain: DomainKind, body: &str, display: Option<&str>) -> ParsedInbound {
        // Subject "Hello" is intentionally neutral — picking
        // anything purchase-intent-flavoured (e.g. "Cotización")
        // would trip the new purchase_intent rule and skew the
        // pre-existing baseline tests.
        parsed_with_subject(domain, "Hello", body, display)
    }

    fn parsed_with_subject(
        domain: DomainKind,
        subject: &str,
        body: &str,
        display: Option<&str>,
    ) -> ParsedInbound {
        ParsedInbound {
            instance: "default".into(),
            account_id: "acct".into(),
            uid: 1,
            from_email: "x@y.com".into(),
            from_display_name: display.map(String::from),
            to_emails: vec![],
            reply_to: None,
            subject: subject.into(),
            message_id: Some("m@x".into()),
            in_reply_to: None,
            references: vec![],
            body_excerpt: body.into(),
            thread_id: "th".into(),
            from_domain_kind: domain,
        }
    }

    #[test]
    fn corporate_substantive_senior_lands_high() {
        // Subject + body are kept neutral so purchase_intent
        // doesn't trip in the "no purchase signal" baseline.
        let body = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega aleph bet gimel dalet he vav".to_string();
        let p = parsed_with_subject(
            DomainKind::Corporate,
            "Hello",
            body.trim(),
            Some("Juan Pérez (CTO)"),
        );
        let s = score_lead(&p);
        // 20 (corporate) + 10 (substantive) + 15 (senior) = 45.
        assert_eq!(s.value(), 45);
        let labels: Vec<&str> = s.reasons().iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"corporate_domain"));
        assert!(labels.contains(&"substantive_body"));
        assert!(labels.contains(&"senior_signature"));
    }

    #[test]
    fn personal_one_word_auto_reply_lands_zero() {
        let p = parsed_with_subject(DomainKind::Personal, "Re:", "ok", None);
        let s = score_lead(&p);
        // -5 (personal) + -10 (brief, 1 word) = -15 → clamped to 0.
        assert_eq!(s.value(), 0);
    }

    #[test]
    fn corporate_brief_lands_at_corporate_minus_brief() {
        let p = parsed_with_subject(DomainKind::Corporate, "Re:", "ok", None);
        let s = score_lead(&p);
        // 20 (corporate) - 10 (brief, 1 word) = 10.
        assert_eq!(s.value(), 10);
    }

    /// Regression test for the "Quiero comprar tu producto" bug:
    /// 4 words of high-intent purchase signal from a personal
    /// domain used to land at 0 (-5 personal + -10 brief). With
    /// the lowered brief threshold + new purchase_intent rule,
    /// the lead now scores high enough to surface.
    #[test]
    fn quiero_comprar_short_personal_email_lands_high() {
        let p = parsed_with_subject(
            DomainKind::Personal,
            "Quiero comprar",
            "Quiero comprar tu producto",
            Some("Cliente"),
        );
        let s = score_lead(&p);
        // -5 (personal) + 25 (purchase_intent: "quiero comprar")
        // = 20. Brief (<3) does NOT fire — body is 4 words.
        assert_eq!(s.value(), 20);
        let labels: Vec<&str> = s.reasons().iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"purchase_intent"));
        assert!(!labels.contains(&"brief_body"));
    }

    #[test]
    fn purchase_intent_in_subject_only_fires() {
        // Body is a polite filler; subject carries the signal.
        let p = parsed_with_subject(
            DomainKind::Personal,
            "Quiero comprar el plan Pro",
            "hola, tengo una pregunta",
            None,
        );
        let s = score_lead(&p);
        let labels: Vec<&str> = s.reasons().iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"purchase_intent"));
    }

    #[test]
    fn purchase_intent_english_keywords_fire() {
        let p = parsed_with_subject(
            DomainKind::Personal,
            "Looking to buy",
            "Hi, looking to buy your service.",
            None,
        );
        let s = score_lead(&p);
        let labels: Vec<&str> = s.reasons().iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"purchase_intent"));
    }

    #[test]
    fn no_purchase_signal_no_boost() {
        let p = parsed_with_subject(
            DomainKind::Corporate,
            "Hola",
            "Hola, mi nombre es Juan y soy de Globex Corp.",
            None,
        );
        let s = score_lead(&p);
        let labels: Vec<&str> = s.reasons().iter().map(|r| r.label.as_str()).collect();
        assert!(!labels.contains(&"purchase_intent"));
    }

    #[test]
    fn brief_threshold_lowered_to_three() {
        // 3 words = boundary, brief should NOT fire.
        let p = parsed_with_subject(DomainKind::Corporate, "x", "uno dos tres", None);
        let s = score_lead(&p);
        let labels: Vec<&str> = s.reasons().iter().map(|r| r.label.as_str()).collect();
        assert!(!labels.contains(&"brief_body"));

        // 2 words = brief still fires.
        let p2 = parsed_with_subject(DomainKind::Corporate, "x", "uno dos", None);
        let s2 = score_lead(&p2);
        let labels2: Vec<&str> = s2.reasons().iter().map(|r| r.label.as_str()).collect();
        assert!(labels2.contains(&"brief_body"));
    }

    #[test]
    fn senior_token_is_case_insensitive() {
        let body = "Lorem ipsum ".repeat(20);
        let p1 = parsed_with(DomainKind::Corporate, body.trim(), Some("CEO Juan"));
        let p2 = parsed_with(DomainKind::Corporate, body.trim(), Some("ceo juan"));
        let p3 = parsed_with(DomainKind::Corporate, body.trim(), Some("CEO Juan"));
        assert_eq!(score_lead(&p1).value(), score_lead(&p2).value());
        assert_eq!(score_lead(&p1).value(), score_lead(&p3).value());
    }

    #[test]
    fn no_display_name_disables_senior_rule() {
        let body = "Lorem ipsum ".repeat(20);
        let p = parsed_with(DomainKind::Corporate, body.trim(), None);
        let s = score_lead(&p);
        // 20 + 10 = 30 (no senior boost).
        assert_eq!(s.value(), 30);
    }

    #[test]
    fn body_words_uses_whitespace_split() {
        // Exactly 30 words → trips the substantive threshold.
        let body = "word ".repeat(30);
        let p = parsed_with(DomainKind::Corporate, body.trim(), None);
        let s = score_lead(&p);
        assert_eq!(s.value(), 30);
        let labels: Vec<&str> = s.reasons().iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"substantive_body"));
    }

    #[test]
    fn build_returns_six_rules() {
        // 5 original rules + 1 new purchase_intent rule.
        let s = build_marketing_scorer();
        assert_eq!(s.rule_count(), 6);
    }

    #[test]
    fn score_carries_operator_facing_detail() {
        let body = "Lorem ipsum ".repeat(20);
        let p = parsed_with(
            DomainKind::Corporate,
            body.trim(),
            Some("CEO Juan"),
        );
        let s = score_lead(&p);
        let detailed = s
            .reasons()
            .iter()
            .filter_map(|r| r.detail.as_deref())
            .collect::<Vec<_>>();
        // Rule details are now in English ("sender from a
        // corporate domain") per the repo language rule.
        assert!(detailed.iter().any(|d| d.contains("corporate")));
    }
}

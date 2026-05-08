//! Cross-channel duplicate person detector (M15.23.e).
//!
//! **F29 sweep:** marketing-specific by design. Orchestrator
//! is CRM-flavoured (email_match / phone_match /
//! name_company_fuzzy signals). Pure helpers
//! (`normalize_name`, `name_jaccard`, `email_domain`) are
//! 30 LOC and stay private — lift candidate when a 2nd
//! consumer surfaces a use case for fuzzy text matching.
//!
//! Given a candidate [`Person`] (just resolved from an
//! inbound email or WhatsApp contact), surfaces other
//! `Person` records on the same tenant that may refer to
//! the same human. The operator UI renders the result as
//! a "merge?" prompt — heuristic only; the operator
//! confirms or dismisses.
//!
//! ## Match signals
//!
//! Three independent lookups, each yielding its own
//! confidence band:
//!
//! - **Email exact** (1.00) — any alt_email of the candidate
//!   already owned by another person via
//!   [`PersonEmailStore::find_owner`].
//! - **Phone exact** (1.00) — caller-supplied phone (e.g.
//!   the candidate's WhatsApp JID, normalised) already
//!   owned by another person via
//!   [`PersonPhoneStore::find_owner`].
//! - **Name + company fuzzy** (0.40 .. 0.90) — same
//!   `(normalized_name, company_id)` pair on a different
//!   `person_id`. Token-level Jaccard on the name; exact
//!   match on the company. Drops below 0.40 ⇒ not a
//!   candidate.
//!
//! Multiple signals on the same `person_id` collapse:
//! caller takes the highest confidence.
//!
//! ## What this does NOT do
//!
//! - Auto-merge. The operator must confirm.
//! - LLM tiebreaker (deferred to M18 when the LLM client
//!   wires through).
//! - Cross-tenant. Tenant boundary is enforced at every
//!   lookup.

use std::collections::HashMap;

use nexo_microapp_sdk::identity::{
    Person, PersonEmailStore, PersonId, PersonPhoneStore, PersonStore,
};

use crate::error::MarketingError;

/// One candidate the matcher surfaced. Sorted desc by
/// `confidence` at the top of the returned vec.
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateCandidate {
    /// Person id of the existing record that may be the same
    /// human.
    pub person_id: PersonId,
    /// 0.0 .. 1.0 confidence band.
    pub confidence: f32,
    /// Stable machine label (`email_match`, `phone_match`,
    /// `name_company_fuzzy`).
    pub signal: String,
    /// Operator-facing one-liner ("matched email
    /// `juan@globex.io`").
    pub detail: String,
}

/// Inputs the matcher needs. Caller bundles to keep the
/// trait surface narrow.
pub struct MatchInputs<'a> {
    /// Candidate person — just resolved from an inbound
    /// email or WA contact. May or may not be persisted yet.
    pub candidate: &'a Person,
    /// Phone identifiers the caller has on the candidate.
    /// Pre-normalised via
    /// `nexo_microapp_sdk::identity::normalize_jid` (or the
    /// caller's own canonicalisation). Empty when scanning
    /// an email-only candidate.
    pub phones: &'a [String],
}

/// Result struct the caller iterates. Vec is sorted desc by
/// confidence; the top entry is the operator's first
/// suggestion.
pub type MatchResult = Vec<DuplicateCandidate>;

/// Fuzzy-match floor — name + company below this drops out
/// of the candidate list entirely (the operator never
/// sees a 0.20 "maybe?" — too noisy).
pub const FUZZY_FLOOR: f32 = 0.40;

/// Run the matcher. Each store is queried with
/// `tenant_id` so cross-tenant leakage is impossible by
/// construction.
///
/// Failures from any one store degrade the corresponding
/// signal silently — a SQLite hiccup on the email lookup
/// shouldn't sink the phone + name signals. The caller
/// gets whatever signals worked.
pub async fn find_duplicate_candidates(
    tenant_id: &str,
    inputs: &MatchInputs<'_>,
    email_store: &dyn PersonEmailStore,
    phone_store: Option<&dyn PersonPhoneStore>,
    person_store: &dyn PersonStore,
) -> Result<MatchResult, MarketingError> {
    let mut by_person: HashMap<PersonId, DuplicateCandidate> = HashMap::new();

    // ── Email exact ─────────────────────────────────────────
    let mut emails: Vec<&str> = Vec::with_capacity(1 + inputs.candidate.alt_emails.len());
    if !inputs.candidate.primary_email.is_empty() {
        emails.push(&inputs.candidate.primary_email);
    }
    emails.extend(inputs.candidate.alt_emails.iter().map(|s| s.as_str()));

    for email in emails {
        match email_store.find_owner(tenant_id, email).await {
            Ok(Some(owner)) if owner != inputs.candidate.id => {
                let detail = format!("matched email `{email}`");
                upsert_candidate(
                    &mut by_person,
                    DuplicateCandidate {
                        person_id: owner,
                        confidence: 1.00,
                        signal: "email_match".into(),
                        detail,
                    },
                );
            }
            // Self-owned or unowned — no candidate.
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    target: "marketing.duplicate",
                    error = %e,
                    email = %email,
                    "email_store.find_owner failed (signal degraded)"
                );
            }
        }
    }

    // ── Phone exact ─────────────────────────────────────────
    if let Some(phones) = phone_store {
        for phone in inputs.phones {
            match phones.find_owner(tenant_id, phone).await {
                Ok(Some(owner)) if owner != inputs.candidate.id => {
                    let detail = format!("matched phone `{phone}`");
                    upsert_candidate(
                        &mut by_person,
                        DuplicateCandidate {
                            person_id: owner,
                            confidence: 1.00,
                            signal: "phone_match".into(),
                            detail,
                        },
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "marketing.duplicate",
                        error = %e,
                        phone = %phone,
                        "phone_store.find_owner failed (signal degraded)"
                    );
                }
            }
        }
    }

    // ── Name + company fuzzy ────────────────────────────────
    //
    // F24 — when the candidate has a `company_id`, scan EVERY
    // person on the same tenant who shares that company via
    // the new `PersonStore::list_by_company` (capped at 200
    // rows; tenants with > 200 same-company persons are rare
    // + the operator UI surfaces only top-confidence
    // candidates anyway). When no `company_id` is set, we
    // degrade to comparing against the candidates already
    // surfaced via email/phone (the previous behaviour).
    //
    // Either path also picks up the `same email domain` boost
    // when the candidate's email lands on a corporate domain.
    let candidate_norm = normalize_name(&inputs.candidate.primary_name);
    if !candidate_norm.is_empty() {
        let candidate_domain = email_domain(&inputs.candidate.primary_email);

        // Build the comparison set.
        let mut comparison_pool: Vec<Person> = Vec::new();
        if let Some(company) = inputs.candidate.company_id.as_ref() {
            match person_store
                .list_by_company(tenant_id, &company.0, 200)
                .await
            {
                Ok(rows) => {
                    for p in rows {
                        if p.id != inputs.candidate.id {
                            comparison_pool.push(p);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "marketing.duplicate",
                        error = %e,
                        company = %company.0,
                        "person_store.list_by_company failed (degrading to email-pool fallback)"
                    );
                }
            }
        }
        // Always also fold in the email/phone-matched persons
        // (catches cross-domain cases where two emails point
        // at the same company but the explicit `company_id`
        // hasn't been resolved yet for one of them).
        for owner_id in by_person.keys().cloned().collect::<Vec<_>>() {
            if comparison_pool.iter().any(|p| p.id == owner_id) {
                continue;
            }
            if let Ok(Some(p)) = person_store.get(tenant_id, &owner_id).await {
                comparison_pool.push(p);
            }
        }

        for p in comparison_pool {
            let other_norm = normalize_name(&p.primary_name);
            let jaccard = name_jaccard(&candidate_norm, &other_norm);
            let same_company = p.company_id == inputs.candidate.company_id
                && p.company_id.is_some();
            let mut conf = jaccard;
            if same_company {
                // Same explicit company → boost.
                conf = (conf + 0.20).min(1.0);
            } else if candidate_domain
                .zip(email_domain(&p.primary_email))
                .map(|(a, b)| a == b)
                .unwrap_or(false)
            {
                // Same email domain (corporate proxy for
                // "same employer") → modest boost.
                conf = (conf + 0.10).min(1.0);
            }
            if conf >= FUZZY_FLOOR {
                let detail = format!(
                    "name `{}` similar to existing `{}`",
                    inputs.candidate.primary_name, p.primary_name,
                );
                upsert_candidate(
                    &mut by_person,
                    DuplicateCandidate {
                        person_id: p.id.clone(),
                        confidence: conf,
                        signal: "name_company_fuzzy".into(),
                        detail,
                    },
                );
            }
        }
    }

    let mut result: MatchResult = by_person.into_values().collect();
    result.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(result)
}

/// Insert `c` into the map, keeping the highest-confidence
/// signal per `person_id`.
fn upsert_candidate(
    map: &mut HashMap<PersonId, DuplicateCandidate>,
    c: DuplicateCandidate,
) {
    map.entry(c.person_id.clone())
        .and_modify(|existing| {
            if c.confidence > existing.confidence {
                *existing = c.clone();
            }
        })
        .or_insert(c);
}

fn normalize_name(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Token-level Jaccard. Both strings already normalized via
/// [`normalize_name`].
fn name_jaccard(a: &str, b: &str) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let sa: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let sb: std::collections::HashSet<&str> = b.split_whitespace().collect();
    let inter = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    if union == 0 {
        0.0
    } else {
        inter as f32 / union as f32
    }
}

fn email_domain(email: &str) -> Option<&str> {
    email.split_once('@').map(|(_, d)| d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use nexo_microapp_sdk::identity::{
        Company, CompanyId, IdentityError, PersonEmail, PersonPhone,
    };
    use nexo_tool_meta::marketing::{EnrichmentStatus, TenantIdRef};
    use std::sync::Mutex;

    // ── Fake stores — minimal in-memory impls ────────────────

    #[derive(Default)]
    struct FakePersonStore {
        rows: Mutex<HashMap<(String, String), Person>>,
    }

    #[async_trait]
    impl PersonStore for FakePersonStore {
        async fn upsert(
            &self,
            tenant: &str,
            p: &Person,
        ) -> Result<Person, IdentityError> {
            self.rows.lock().unwrap().insert(
                (tenant.to_string(), p.id.0.clone()),
                p.clone(),
            );
            Ok(p.clone())
        }
        async fn get(
            &self,
            tenant: &str,
            id: &PersonId,
        ) -> Result<Option<Person>, IdentityError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .get(&(tenant.to_string(), id.0.clone()))
                .cloned())
        }
        async fn find_by_email(
            &self,
            _t: &str,
            _e: &str,
        ) -> Result<Option<Person>, IdentityError> {
            Ok(None)
        }
        async fn list_by_company(
            &self,
            tenant: &str,
            company_id: &str,
            limit: usize,
        ) -> Result<Vec<Person>, IdentityError> {
            let map = self.rows.lock().unwrap();
            let mut hits: Vec<Person> = map
                .iter()
                .filter(|((t, _), p)| {
                    t == tenant
                        && p.company_id
                            .as_ref()
                            .map(|c| c.0 == company_id)
                            .unwrap_or(false)
                })
                .map(|(_, p)| p.clone())
                .collect();
            // Mirror the SQLite impl's recency ordering for
            // realistic test behaviour.
            hits.sort_by(|a, b| b.last_seen_at_ms.cmp(&a.last_seen_at_ms));
            hits.truncate(limit);
            Ok(hits)
        }
        async fn delete_by_tenant(
            &self,
            _t: &str,
        ) -> Result<u64, IdentityError> {
            Ok(0)
        }
    }

    #[derive(Default)]
    struct FakeEmailStore {
        rows: Mutex<HashMap<(String, String), PersonId>>,
    }

    #[async_trait]
    impl PersonEmailStore for FakeEmailStore {
        async fn add(
            &self,
            tenant: &str,
            person: &PersonId,
            email: &str,
            _v: bool,
        ) -> Result<PersonEmail, IdentityError> {
            self.rows.lock().unwrap().insert(
                (tenant.to_string(), email.to_string()),
                person.clone(),
            );
            Ok(PersonEmail {
                person_id: person.clone(),
                tenant_id: TenantIdRef(tenant.into()),
                email: email.into(),
                verified: false,
                added_at_ms: 0,
            })
        }
        async fn list_for_person(
            &self,
            _t: &str,
            _p: &PersonId,
        ) -> Result<Vec<PersonEmail>, IdentityError> {
            Ok(vec![])
        }
        async fn find_owner(
            &self,
            tenant: &str,
            email: &str,
        ) -> Result<Option<PersonId>, IdentityError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .get(&(tenant.to_string(), email.to_string()))
                .cloned())
        }
        async fn delete_by_tenant(
            &self,
            _t: &str,
        ) -> Result<u64, IdentityError> {
            Ok(0)
        }
    }

    #[derive(Default)]
    struct FakePhoneStore {
        rows: Mutex<HashMap<(String, String), PersonId>>,
    }

    #[async_trait]
    impl PersonPhoneStore for FakePhoneStore {
        async fn add(
            &self,
            tenant: &str,
            person: &PersonId,
            phone: &str,
            _v: bool,
        ) -> Result<PersonPhone, IdentityError> {
            self.rows.lock().unwrap().insert(
                (tenant.to_string(), phone.to_string()),
                person.clone(),
            );
            Ok(PersonPhone {
                person_id: person.clone(),
                tenant_id: TenantIdRef(tenant.into()),
                phone: phone.into(),
                verified: false,
                added_at_ms: 0,
            })
        }
        async fn list_for_person(
            &self,
            _t: &str,
            _p: &PersonId,
        ) -> Result<Vec<PersonPhone>, IdentityError> {
            Ok(vec![])
        }
        async fn find_owner(
            &self,
            tenant: &str,
            phone: &str,
        ) -> Result<Option<PersonId>, IdentityError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .get(&(tenant.to_string(), phone.to_string()))
                .cloned())
        }
        async fn delete_by_tenant(
            &self,
            _t: &str,
        ) -> Result<u64, IdentityError> {
            Ok(0)
        }
    }

    fn person(id: &str, name: &str, email: &str, company: Option<&str>) -> Person {
        Person {
            id: PersonId(id.into()),
            tenant_id: TenantIdRef("acme".into()),
            primary_name: name.into(),
            primary_email: email.into(),
            alt_emails: vec![],
            company_id: company.map(|c| CompanyId(c.into())),
            enrichment_status: EnrichmentStatus::None,
            enrichment_confidence: 0.0,
            tags: vec![],
            created_at_ms: 0,
            last_seen_at_ms: 0,
        }
    }

    // ── normalize_name + name_jaccard ─────────────────────────

    #[test]
    fn normalize_strips_accents_via_lowercase() {
        // Accents survive (lowercase preserves codepoints) —
        // intentional, since two senders with the same
        // accented name should still match.
        assert_eq!(normalize_name("Juan Pérez"), "juan pérez");
    }

    #[test]
    fn normalize_collapses_whitespace_and_punctuation() {
        assert_eq!(
            normalize_name("Juan-Carlos  Pérez,  Jr."),
            "juan carlos pérez jr",
        );
    }

    #[test]
    fn jaccard_full_overlap_returns_one() {
        assert_eq!(name_jaccard("juan perez", "juan perez"), 1.0);
    }

    #[test]
    fn jaccard_no_overlap_returns_zero() {
        assert_eq!(name_jaccard("juan", "ana"), 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        // {juan, perez} ∩ {juan, garcia} = {juan} (1)
        // {juan, perez, garcia} ∪ count = 3 → 1/3.
        let j = name_jaccard("juan perez", "juan garcia");
        assert!((j - (1.0 / 3.0)).abs() < 0.01);
    }

    // ── Matcher integration ──────────────────────────────────

    async fn fixture_with_existing_person() -> (
        FakePersonStore,
        FakeEmailStore,
        FakePhoneStore,
    ) {
        let pstore = FakePersonStore::default();
        let estore = FakeEmailStore::default();
        let phstore = FakePhoneStore::default();
        // Existing person: Juan @ Globex.
        let existing = person(
            "juan-existing",
            "Juan Pérez",
            "juan@globex.io",
            Some("globex"),
        );
        pstore.upsert("acme", &existing).await.unwrap();
        estore
            .add("acme", &existing.id, "juan@globex.io", true)
            .await
            .unwrap();
        phstore
            .add("acme", &existing.id, "573001234567@s.whatsapp.net", true)
            .await
            .unwrap();
        (pstore, estore, phstore)
    }

    #[tokio::test]
    async fn email_exact_match_surfaces_existing_person() {
        let (ps, es, phs) = fixture_with_existing_person().await;
        // New candidate with same email.
        let candidate = person(
            "juan-new",
            "Juan P.",
            "juan@globex.io",
            None,
        );
        let phones: Vec<String> = vec![];
        let r = find_duplicate_candidates(
            "acme",
            &MatchInputs {
                candidate: &candidate,
                phones: &phones,
            },
            &es,
            Some(&phs),
            &ps,
        )
        .await
        .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].person_id.0, "juan-existing");
        assert_eq!(r[0].confidence, 1.0);
        assert_eq!(r[0].signal, "email_match");
    }

    #[tokio::test]
    async fn phone_exact_match_surfaces_via_jid() {
        let (ps, es, phs) = fixture_with_existing_person().await;
        // Candidate is the WA-side contact. Email empty.
        let mut candidate = person("juan-wa", "Juan", "", None);
        candidate.primary_email = String::new();
        let phones = vec!["573001234567@s.whatsapp.net".to_string()];
        let r = find_duplicate_candidates(
            "acme",
            &MatchInputs {
                candidate: &candidate,
                phones: &phones,
            },
            &es,
            Some(&phs),
            &ps,
        )
        .await
        .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].person_id.0, "juan-existing");
        assert_eq!(r[0].signal, "phone_match");
    }

    #[tokio::test]
    async fn self_match_filtered_out() {
        let (ps, es, phs) = fixture_with_existing_person().await;
        // Candidate IS the existing person — must NOT
        // surface itself as a duplicate.
        let candidate = person(
            "juan-existing",
            "Juan Pérez",
            "juan@globex.io",
            Some("globex"),
        );
        let phones: Vec<String> = vec![];
        let r = find_duplicate_candidates(
            "acme",
            &MatchInputs {
                candidate: &candidate,
                phones: &phones,
            },
            &es,
            Some(&phs),
            &ps,
        )
        .await
        .unwrap();
        assert!(r.is_empty(), "got {r:?}");
    }

    #[tokio::test]
    async fn cross_tenant_isolation() {
        let (ps, es, phs) = fixture_with_existing_person().await;
        // Same email, different tenant — must NOT match.
        let candidate = person(
            "juan-new",
            "Juan",
            "juan@globex.io",
            None,
        );
        let phones: Vec<String> = vec![];
        let r = find_duplicate_candidates(
            "globex",
            &MatchInputs {
                candidate: &candidate,
                phones: &phones,
            },
            &es,
            Some(&phs),
            &ps,
        )
        .await
        .unwrap();
        assert!(r.is_empty());
    }

    #[tokio::test]
    async fn no_phone_store_still_works() {
        let (ps, es, _phs) = fixture_with_existing_person().await;
        let candidate = person(
            "juan-new",
            "Juan",
            "juan@globex.io",
            None,
        );
        let phones: Vec<String> = vec![];
        let r = find_duplicate_candidates(
            "acme",
            &MatchInputs {
                candidate: &candidate,
                phones: &phones,
            },
            &es,
            None,
            &ps,
        )
        .await
        .unwrap();
        // Email match still fires.
        assert_eq!(r.len(), 1);
    }

    #[tokio::test]
    async fn alt_emails_are_scanned() {
        let (ps, es, phs) = fixture_with_existing_person().await;
        let mut candidate = person(
            "juan-new",
            "Juan",
            "different@x.com",
            None,
        );
        candidate.alt_emails = vec!["juan@globex.io".into()];
        let phones: Vec<String> = vec![];
        let r = find_duplicate_candidates(
            "acme",
            &MatchInputs {
                candidate: &candidate,
                phones: &phones,
            },
            &es,
            Some(&phs),
            &ps,
        )
        .await
        .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].signal, "email_match");
    }

    #[tokio::test]
    async fn email_and_phone_collapse_to_one_candidate() {
        let (ps, es, phs) = fixture_with_existing_person().await;
        let candidate = person(
            "juan-new",
            "Juan",
            "juan@globex.io",
            None,
        );
        let phones = vec!["573001234567@s.whatsapp.net".to_string()];
        let r = find_duplicate_candidates(
            "acme",
            &MatchInputs {
                candidate: &candidate,
                phones: &phones,
            },
            &es,
            Some(&phs),
            &ps,
        )
        .await
        .unwrap();
        // One person → one candidate row, even though two
        // signals match.
        assert_eq!(r.len(), 1);
        // Either signal could win — both at 1.0.
        assert_eq!(r[0].confidence, 1.0);
    }

    #[tokio::test]
    async fn unrelated_candidate_returns_empty() {
        let (ps, es, phs) = fixture_with_existing_person().await;
        let candidate = person(
            "ghost",
            "Maria Rodriguez",
            "maria@unrelated.com",
            None,
        );
        let phones = vec!["111111111@s.whatsapp.net".to_string()];
        let r = find_duplicate_candidates(
            "acme",
            &MatchInputs {
                candidate: &candidate,
                phones: &phones,
            },
            &es,
            Some(&phs),
            &ps,
        )
        .await
        .unwrap();
        assert!(r.is_empty());
    }

    // ─── F24 — broadened name+company search ──────────────────

    #[tokio::test]
    async fn fuzzy_finds_match_via_company_listing_without_email_overlap() {
        // F24 — pre-broadening, this candidate would NOT
        // surface because no email or phone match seeds the
        // comparison pool. The new
        // `PersonStore::list_by_company` call brings in every
        // `globex` person; Jaccard + same-company boost lifts
        // the existing "Juan Pérez" above the fuzzy floor.
        let ps = FakePersonStore::default();
        let es = FakeEmailStore::default();
        let phs = FakePhoneStore::default();
        // Existing Juan @ globex.
        let existing = person(
            "juan-existing",
            "Juan Pérez",
            "juan@globex.io",
            Some("globex"),
        );
        ps.upsert("acme", &existing).await.unwrap();
        // Candidate: same name + same company, but a totally
        // different email (globex.com instead of globex.io —
        // domain doesn't even match the existing record's).
        // Email + phone signals miss; only the broadened
        // company scan can surface the duplicate.
        let candidate = person(
            "juan-new",
            "Juan Pérez",
            "juan@globex.com",
            Some("globex"),
        );
        let phones: Vec<String> = vec![];
        let r = find_duplicate_candidates(
            "acme",
            &MatchInputs {
                candidate: &candidate,
                phones: &phones,
            },
            &es,
            Some(&phs),
            &ps,
        )
        .await
        .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].person_id.0, "juan-existing");
        assert_eq!(r[0].signal, "name_company_fuzzy");
        // 1.0 Jaccard on 2-token names + 0.20 same-company
        // boost saturates to 1.0.
        assert!(r[0].confidence >= 0.99);
    }

    #[tokio::test]
    async fn fuzzy_company_scan_excludes_self() {
        // Candidate IS the company-listed row. Must NOT
        // surface itself.
        let ps = FakePersonStore::default();
        let es = FakeEmailStore::default();
        let phs = FakePhoneStore::default();
        let row = person(
            "juan",
            "Juan Pérez",
            "juan@globex.io",
            Some("globex"),
        );
        ps.upsert("acme", &row).await.unwrap();
        let phones: Vec<String> = vec![];
        let r = find_duplicate_candidates(
            "acme",
            &MatchInputs {
                candidate: &row,
                phones: &phones,
            },
            &es,
            Some(&phs),
            &ps,
        )
        .await
        .unwrap();
        assert!(r.is_empty(), "got {r:?}");
    }

    #[tokio::test]
    async fn fuzzy_company_scan_below_floor_drops_candidate() {
        // Existing row has same company but a name that
        // shares zero tokens — Jaccard 0 + 0.20 boost = 0.20
        // which is below the 0.40 fuzzy floor. Must NOT
        // surface.
        let ps = FakePersonStore::default();
        let es = FakeEmailStore::default();
        let phs = FakePhoneStore::default();
        ps.upsert(
            "acme",
            &person(
                "ghost",
                "Maria Rodriguez",
                "maria@globex.io",
                Some("globex"),
            ),
        )
        .await
        .unwrap();
        let candidate = person(
            "juan-new",
            "Juan Pérez",
            "juan@globex.io",
            Some("globex"),
        );
        let phones: Vec<String> = vec![];
        let r = find_duplicate_candidates(
            "acme",
            &MatchInputs {
                candidate: &candidate,
                phones: &phones,
            },
            &es,
            Some(&phs),
            &ps,
        )
        .await
        .unwrap();
        assert!(
            r.iter()
                .all(|c| c.signal != "name_company_fuzzy"),
            "fuzzy below floor must not surface: {r:?}",
        );
    }

    #[tokio::test]
    async fn no_company_id_falls_back_to_email_pool() {
        // Candidate has no `company_id` — F24 broadening
        // can't fire. The matcher falls back to the
        // pre-F24 behaviour: only email/phone-matched
        // persons are compared.
        let ps = FakePersonStore::default();
        let es = FakeEmailStore::default();
        let phs = FakePhoneStore::default();
        let existing = person(
            "juan-existing",
            "Juan Pérez",
            "juan@globex.io",
            None,
        );
        ps.upsert("acme", &existing).await.unwrap();
        es.add("acme", &existing.id, "juan@globex.io", true)
            .await
            .unwrap();
        let candidate = person(
            "juan-new",
            "Juan P.",
            "juan@globex.io",
            None,
        );
        let phones: Vec<String> = vec![];
        let r = find_duplicate_candidates(
            "acme",
            &MatchInputs {
                candidate: &candidate,
                phones: &phones,
            },
            &es,
            Some(&phs),
            &ps,
        )
        .await
        .unwrap();
        // Email match still fires; the fuzzy signal also
        // surfaces (same email domain boost).
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].person_id.0, "juan-existing");
    }
}

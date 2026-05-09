//! Cross-thread linker. Looks up the inbound email in the
//! per-tenant `person_email` table — if a previous thread
//! already mapped that address to a person, hit at confidence
//! 0.95 (deterministic). Free; cache-cheap.
//!
//! **F29 sweep:** marketing-specific impl of the SDK's
//! `EnrichmentSource`. Reads the lifted `PersonEmailStore`
//! (`nexo-microapp-sdk::identity`) but emits CRM-shaped
//! `EnrichmentResult` rows.

use async_trait::async_trait;

use nexo_microapp_sdk::enrichment::{
    EnrichmentInput, EnrichmentSource, EnrichmentSourceError, SourceCost,
};
use nexo_microapp_sdk::identity::{PersonEmailStore, PersonStore};
use nexo_tool_meta::marketing::{CompanyInferred, EnrichmentResult, PersonInferred};

pub struct CrossThreadLinker<P, E>
where
    P: PersonStore + 'static,
    E: PersonEmailStore + 'static,
{
    persons: P,
    person_emails: E,
    tenant_id: String,
}

impl<P, E> CrossThreadLinker<P, E>
where
    P: PersonStore + 'static,
    E: PersonEmailStore + 'static,
{
    pub fn new(persons: P, person_emails: E, tenant_id: String) -> Self {
        Self {
            persons,
            person_emails,
            tenant_id,
        }
    }
}

#[async_trait]
impl<P, E> EnrichmentSource for CrossThreadLinker<P, E>
where
    P: PersonStore + 'static,
    E: PersonEmailStore + 'static,
{
    fn name(&self) -> &str {
        "cross_thread"
    }
    fn cost_estimate(&self) -> SourceCost {
        SourceCost::Cheap
    }
    async fn extract(
        &self,
        input: &EnrichmentInput<'_>,
    ) -> Result<Option<EnrichmentResult>, EnrichmentSourceError> {
        let owner = self
            .person_emails
            .find_owner(&self.tenant_id, input.from_email)
            .await
            .map_err(|e| EnrichmentSourceError::SourceUnavailable {
                source_name: "cross_thread".into(),
                reason: e.to_string(),
            })?;
        let person_id = match owner {
            Some(id) => id,
            None => return Ok(None),
        };
        let person = self
            .persons
            .get(&self.tenant_id, &person_id)
            .await
            .map_err(|e| EnrichmentSourceError::SourceUnavailable {
                source_name: "cross_thread".into(),
                reason: e.to_string(),
            })?;
        let person = match person {
            Some(p) => p,
            // Email mapped but the person row was deleted —
            // treat as miss; the resolver will create fresh.
            None => return Ok(None),
        };
        Ok(Some(EnrichmentResult {
            source: "cross_thread".into(),
            confidence: 0.95,
            person_inferred: Some(PersonInferred {
                name: Some(person.primary_name),
                role: None,
                seniority: None,
            }),
            company_inferred: person.company_id.map(|cid| CompanyInferred {
                name: Some(cid.0.clone()),
                domain: None,
                industry: None,
            }),
            note: Some("matched prior thread for same email address".into()),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexo_microapp_sdk::identity::{
        open_pool, SqlitePersonEmailStore, SqlitePersonStore,
    };
    use nexo_tool_meta::marketing::{CompanyId, EnrichmentStatus, Person, PersonId, TenantIdRef};

    async fn seed_person(
        persons: &SqlitePersonStore,
        emails: &SqlitePersonEmailStore,
        tenant: &str,
        person_id: &str,
        name: &str,
        primary_email: &str,
        alt_email: Option<&str>,
        company: Option<&str>,
    ) {
        persons
            .upsert(
                tenant,
                &Person {
                    id: PersonId(person_id.into()),
                    tenant_id: TenantIdRef(tenant.into()),
                    primary_name: name.into(),
                    primary_email: primary_email.into(),
                    alt_emails: vec![],
                    company_id: company.map(|c| CompanyId(c.into())),
                    enrichment_status: EnrichmentStatus::Manual,
                    enrichment_confidence: 1.0,
                    tags: vec![],
                    created_at_ms: 0,
                    last_seen_at_ms: 0,
                },
            )
            .await
            .unwrap();
        emails
            .add(tenant, &PersonId(person_id.into()), primary_email, true)
            .await
            .unwrap();
        if let Some(e) = alt_email {
            emails
                .add(tenant, &PersonId(person_id.into()), e, true)
                .await
                .unwrap();
        }
    }

    fn inp<'a>(email: &'a str) -> EnrichmentInput<'a> {
        EnrichmentInput {
            from_email: email,
            from_display_name: None,
            subject: "",
            body_excerpt: "",
            reply_to: None,
        }
    }

    #[tokio::test]
    async fn miss_when_email_not_in_table() {
        let pool = open_pool(std::path::Path::new(":memory:")).await.unwrap();
        let persons = SqlitePersonStore::new(pool.clone());
        let emails = SqlitePersonEmailStore::new(pool);
        let linker = CrossThreadLinker::new(persons, emails, "acme".into());
        let r = linker.extract(&inp("ghost@gmail.com")).await.unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn hit_when_email_already_known_via_alt() {
        let pool = open_pool(std::path::Path::new(":memory:")).await.unwrap();
        let persons = SqlitePersonStore::new(pool.clone());
        let emails = SqlitePersonEmailStore::new(pool.clone());
        seed_person(
            &persons,
            &emails,
            "acme",
            "juan",
            "Juan García",
            "juan@acme.com",
            Some("juan.alt@gmail.com"),
            Some("acme-corp"),
        )
        .await;
        let linker = CrossThreadLinker::new(persons, emails, "acme".into());
        let r = linker.extract(&inp("juan.alt@gmail.com")).await.unwrap().unwrap();
        assert_eq!(r.confidence, 0.95);
        assert_eq!(
            r.person_inferred.unwrap().name.unwrap(),
            "Juan García"
        );
        assert_eq!(
            r.company_inferred.unwrap().name.unwrap(),
            "acme-corp"
        );
    }

    #[tokio::test]
    async fn cross_tenant_isolation() {
        let pool = open_pool(std::path::Path::new(":memory:")).await.unwrap();
        let persons = SqlitePersonStore::new(pool.clone());
        let emails = SqlitePersonEmailStore::new(pool.clone());
        // Same email known in tenant A, NOT known in tenant B.
        seed_person(
            &persons,
            &emails,
            "acme",
            "juan",
            "Juan",
            "juan@gmail.com",
            None,
            None,
        )
        .await;
        let linker_globex = CrossThreadLinker::new(persons, emails, "globex".into());
        let r = linker_globex.extract(&inp("juan@gmail.com")).await.unwrap();
        // Tenant globex's linker MUST NOT see tenant acme's
        // person_email row.
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn email_mapped_but_person_deleted_treated_as_miss() {
        let pool = open_pool(std::path::Path::new(":memory:")).await.unwrap();
        let persons = SqlitePersonStore::new(pool.clone());
        let emails = SqlitePersonEmailStore::new(pool.clone());
        emails
            .add("acme", &PersonId("juan".into()), "juan@gmail.com", true)
            .await
            .unwrap();
        // No person row inserted — represents a stale mapping
        // (should never happen in prod thanks to FK, but the
        // adapter must not crash if it does).
        let linker = CrossThreadLinker::new(persons, emails, "acme".into());
        let r = linker.extract(&inp("juan@gmail.com")).await.unwrap();
        assert!(r.is_none());
    }
}

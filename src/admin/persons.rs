//! M15.21.b — Person + enrichment surface for the lead
//! drawer.
//!
//! Two endpoints:
//!
//! - `GET /persons/:person_id` — returns the Person row
//!   (with current enrichment state) plus the linked
//!   Company row when present. The lead drawer renders
//!   the Confirm/Edit/Discard prompt off this payload.
//! - `POST /persons/:person_id/confirm-enrichment` —
//!   operator-driven manual confirmation. Sets the
//!   person's `enrichment_status = Manual`,
//!   `enrichment_confidence = 1.0`, optionally updates
//!   `primary_name`, and optionally creates / links a
//!   Company by name (domain inferred from the person's
//!   primary email when the company doesn't already
//!   exist).
//!
//! Multi-tenant safety: every read + write goes through
//! the auth-middleware-stamped `Extension<TenantId>`. The
//! stores are tenant-keyed; cross-tenant queries return
//! `None`.

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use nexo_microapp_sdk::identity::{CompanyStore, PersonStore};
use nexo_tool_meta::marketing::{
    Company, CompanyId, EnrichmentStatus, PersonId, TenantIdRef,
};

use super::AdminState;
use crate::tenant::TenantId;

/// `GET /persons/:person_id` — single fetch of the Person
/// row plus the linked Company (when set + the company
/// store is wired). `404` when the person is missing for
/// the active tenant — the lead drawer falls back to the
/// "no enrichment data" placeholder.
pub async fn get_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(person_id): Path<String>,
) -> Response {
    let Some(persons) = state.persons.as_ref() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "persons_not_loaded",
            "person store not mounted on AdminState",
        );
    };
    let id = PersonId(person_id.clone());
    let person = match persons.get(tenant_id.as_str(), &id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return error(
                StatusCode::NOT_FOUND,
                "person_not_found",
                &format!("no person with id {person_id:?} for the active tenant"),
            );
        }
        Err(e) => return server_error("person_store", &e.to_string()),
    };
    let company = if let (Some(cs), Some(cid)) =
        (state.companies.as_ref(), person.company_id.as_ref())
    {
        match cs.get(tenant_id.as_str(), cid).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "extension.marketing.persons",
                    error = %e,
                    "company lookup failed during person fetch (non-fatal)"
                );
                None
            }
        }
    } else {
        None
    };
    ok(json!({
        "person": person,
        "company": company,
    }))
}

#[derive(Debug, Default, Deserialize)]
pub struct ConfirmEnrichmentBody {
    /// Optional operator override for the person's display
    /// name. Empty / whitespace-only ⇒ leave the existing
    /// name untouched.
    #[serde(default)]
    pub primary_name: Option<String>,
    /// Optional operator-confirmed company name. Triggers
    /// company upsert keyed by `(tenant_id, domain)` where
    /// the domain is inferred from the person's primary
    /// email. Empty / whitespace-only ⇒ no company write
    /// (the manual confirmation still flips the status to
    /// `Manual` so the resolver doesn't re-prompt).
    #[serde(default)]
    pub company_name: Option<String>,
    /// Optional operator override for the company domain
    /// when the email-derived domain isn't representative
    /// (e.g., a personal email for a corporate contact).
    /// Falls back to the inferred domain when omitted.
    #[serde(default)]
    pub company_domain: Option<String>,
}

/// `POST /persons/:person_id/confirm-enrichment` —
/// operator confirms / edits the inferred enrichment.
/// Always sets `enrichment_status = Manual` +
/// `enrichment_confidence = 1.0`. Returns the updated
/// `{ person, company }` envelope so the frontend can
/// re-render the lead drawer without a follow-up GET.
///
/// Typed errors:
/// - `503 persons_not_loaded` — store not wired
/// - `404 person_not_found` — wrong tenant / id
/// - `503 companies_not_loaded` — caller passed
///   `company_name` but no company store is wired
/// - `400 invalid_company_domain` — domain blank when no
///   email-derived fallback exists
pub async fn confirm_enrichment_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(person_id): Path<String>,
    payload: Option<Json<ConfirmEnrichmentBody>>,
) -> Response {
    let Some(persons) = state.persons.as_ref() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "persons_not_loaded",
            "person store not mounted on AdminState",
        );
    };
    let id = PersonId(person_id.clone());
    let mut person = match persons.get(tenant_id.as_str(), &id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return error(
                StatusCode::NOT_FOUND,
                "person_not_found",
                &format!("no person with id {person_id:?} for the active tenant"),
            );
        }
        Err(e) => return server_error("person_store", &e.to_string()),
    };
    let body_in = payload.map(|Json(b)| b).unwrap_or_default();
    let trimmed_name = body_in
        .primary_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let trimmed_company = body_in
        .company_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let trimmed_domain = body_in
        .company_domain
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase);

    // ── Optional company upsert ─────────────────────────────
    let company_after = if let Some(name) = trimmed_company.as_deref() {
        let Some(companies) = state.companies.as_ref() else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "companies_not_loaded",
                "company store not mounted; pass company_name only when companies are wired",
            );
        };
        let domain = trimmed_domain
            .clone()
            .or_else(|| derive_domain_from_email(&person.primary_email));
        let Some(domain) = domain else {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_company_domain",
                "company_domain is required when the person's primary_email lacks a domain part",
            );
        };
        let now_ms = Utc::now().timestamp_millis();
        // Look up by domain first so re-confirming the same
        // company across leads collapses onto a single row.
        let existing = match companies
            .find_by_domain(tenant_id.as_str(), &domain)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                return server_error("company_store_find", &e.to_string());
            }
        };
        let target = match existing {
            Some(mut row) => {
                // Refresh the operator-confirmed name + bump
                // the enriched_at_ms stamp so the analytics
                // panel knows when the row was last touched.
                row.name = name.into();
                row.enriched_at_ms = Some(now_ms);
                row
            }
            None => Company {
                id: CompanyId(format!("co-{}", uuid::Uuid::new_v4())),
                tenant_id: TenantIdRef(tenant_id.as_str().to_string()),
                domain: domain.clone(),
                name: name.into(),
                industry: None,
                size_band: None,
                enriched_at_ms: Some(now_ms),
                // Operator-driven ⇒ never personal; the
                // resolver's domain classifier flagged this
                // earlier and the operator overrode it.
                is_personal_domain: false,
            },
        };
        match companies.upsert(tenant_id.as_str(), &target).await {
            Ok(c) => {
                person.company_id = Some(c.id.clone());
                Some(c)
            }
            Err(e) => {
                return server_error("company_store_upsert", &e.to_string());
            }
        }
    } else {
        // No company write requested — keep whatever's
        // already linked + return its current row so the
        // frontend doesn't have to re-fetch.
        if let (Some(cs), Some(cid)) =
            (state.companies.as_ref(), person.company_id.as_ref())
        {
            cs.get(tenant_id.as_str(), cid).await.unwrap_or(None)
        } else {
            None
        }
    };

    // ── Person update ───────────────────────────────────────
    if let Some(new_name) = trimmed_name {
        person.primary_name = new_name;
    }
    person.enrichment_status = EnrichmentStatus::Manual;
    person.enrichment_confidence = 1.0;
    person.last_seen_at_ms = Utc::now().timestamp_millis();

    let updated = match persons.upsert(tenant_id.as_str(), &person).await {
        Ok(p) => p,
        Err(e) => return server_error("person_store_upsert", &e.to_string()),
    };

    ok(json!({
        "person": updated,
        "company": company_after,
    }))
}

/// Pull the domain part out of an email address. Returns
/// lower-cased + None for malformed inputs.
fn derive_domain_from_email(email: &str) -> Option<String> {
    let (_, domain) = email.split_once('@')?;
    let trimmed = domain.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

fn ok(result: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "result": result })),
    )
        .into_response()
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "ok": false, "error": { "code": code, "message": message } })),
    )
        .into_response()
}

fn server_error(code: &str, message: &str) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::router;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request};
    use crate::lead::LeadStore;
    use nexo_microapp_sdk::identity::{
        SqliteCompanyStore, SqlitePersonStore,
    };
    use nexo_tool_meta::marketing::Person;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;
    use std::path::PathBuf;
    use tower::util::ServiceExt;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS persons (
                tenant_id TEXT NOT NULL,
                id TEXT NOT NULL,
                primary_name TEXT NOT NULL,
                primary_email TEXT NOT NULL,
                company_id TEXT,
                enrichment_status TEXT NOT NULL,
                enrichment_confidence REAL NOT NULL,
                tags_json TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                last_seen_at_ms INTEGER NOT NULL,
                PRIMARY KEY (tenant_id, id)
            );
            CREATE TABLE IF NOT EXISTS companies (
                tenant_id TEXT NOT NULL,
                id TEXT NOT NULL,
                domain TEXT NOT NULL,
                name TEXT NOT NULL,
                industry TEXT,
                size_band TEXT,
                enriched_at_ms INTEGER,
                is_personal_domain INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (tenant_id, id)
            );
            CREATE INDEX IF NOT EXISTS idx_companies_domain
                ON companies (tenant_id, domain);",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    fn person_fixture(id: &str, email: &str) -> Person {
        Person {
            id: PersonId(id.into()),
            tenant_id: TenantIdRef("acme".into()),
            primary_name: "Juan G".into(),
            primary_email: email.into(),
            alt_emails: vec![],
            company_id: None,
            enrichment_status: EnrichmentStatus::SignatureParsed,
            enrichment_confidence: 0.62,
            tags: vec![],
            created_at_ms: 100,
            last_seen_at_ms: 100,
        }
    }

    async fn build_state(
        with_companies: bool,
    ) -> (Arc<AdminState>, Arc<dyn PersonStore>) {
        let pool = fresh_pool().await;
        let persons: Arc<dyn PersonStore> =
            Arc::new(SqlitePersonStore::new(pool.clone()));
        persons
            .upsert(
                "acme",
                &person_fixture("juan", "juan@acme.com"),
            )
            .await
            .unwrap();
        // Auth middleware refuses any tenant without a
        // mounted LeadStore — wire an in-mem one so the
        // bearer + X-Tenant-Id round-trip lands the
        // request at the persons handler.
        let lead_store = LeadStore::open(
            PathBuf::from(":memory:"),
            TenantId::new("acme").unwrap(),
        )
        .await
        .unwrap();
        let mut state = AdminState::new("secret".into())
            .with_store(Arc::new(lead_store))
            .with_persons(persons.clone());
        if with_companies {
            let companies: Arc<dyn CompanyStore> =
                Arc::new(SqliteCompanyStore::new(pool));
            state = state.with_companies(companies);
        }
        (Arc::new(state), persons)
    }

    fn req_get(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("X-Tenant-Id", "acme")
            .body(Body::empty())
            .unwrap()
    }

    fn req_post(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("X-Tenant-Id", "acme")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    async fn body_to_json(resp: Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn get_returns_person_payload() {
        let (state, _persons) = build_state(true).await;
        let app = router(state);
        let resp = app.oneshot(req_get("/persons/juan")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["person"]["id"], "juan");
        assert_eq!(v["result"]["person"]["enrichment_status"], "signature_parsed");
        assert!(v["result"]["company"].is_null());
    }

    #[tokio::test]
    async fn get_returns_404_for_missing() {
        let (state, _) = build_state(true).await;
        let app = router(state);
        let resp = app.oneshot(req_get("/persons/ghost")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_returns_503_when_persons_not_wired() {
        let lead_store = LeadStore::open(
            PathBuf::from(":memory:"),
            TenantId::new("acme").unwrap(),
        )
        .await
        .unwrap();
        let state = Arc::new(
            AdminState::new("secret".into())
                .with_store(Arc::new(lead_store)),
        );
        let app = router(state);
        let resp = app.oneshot(req_get("/persons/juan")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "persons_not_loaded");
    }

    #[tokio::test]
    async fn confirm_marks_manual_with_full_confidence() {
        let (state, persons) = build_state(true).await;
        let app = router(state);
        let resp = app
            .oneshot(req_post(
                "/persons/juan/confirm-enrichment",
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["person"]["enrichment_status"], "manual");
        let conf = v["result"]["person"]["enrichment_confidence"]
            .as_f64()
            .unwrap();
        assert!((conf - 1.0).abs() < 1e-6);
        // Re-read confirms the persisted value.
        let p = persons
            .get("acme", &PersonId("juan".into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.enrichment_status, EnrichmentStatus::Manual);
        assert!((p.enrichment_confidence - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn confirm_overrides_primary_name_when_provided() {
        let (state, _persons) = build_state(true).await;
        let app = router(state);
        let resp = app
            .oneshot(req_post(
                "/persons/juan/confirm-enrichment",
                json!({ "primary_name": "Juan García" }),
            ))
            .await
            .unwrap();
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["person"]["primary_name"], "Juan García");
    }

    #[tokio::test]
    async fn confirm_creates_company_from_email_domain() {
        let (state, _persons) = build_state(true).await;
        let app = router(state);
        let resp = app
            .oneshot(req_post(
                "/persons/juan/confirm-enrichment",
                json!({ "company_name": "Acme Corp" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        let c = &v["result"]["company"];
        assert_eq!(c["domain"], "acme.com");
        assert_eq!(c["name"], "Acme Corp");
        assert!(c["id"].as_str().unwrap().starts_with("co-"));
        // Person now linked to the company.
        assert_eq!(
            v["result"]["person"]["company_id"].as_str().unwrap(),
            c["id"].as_str().unwrap(),
        );
    }

    #[tokio::test]
    async fn confirm_reuses_existing_company_by_domain() {
        let (state, _persons) = build_state(true).await;
        let app = router(state.clone());
        // First confirmation creates the row.
        let _ = app
            .oneshot(req_post(
                "/persons/juan/confirm-enrichment",
                json!({ "company_name": "Acme Corp" }),
            ))
            .await
            .unwrap();
        // Second confirmation against a different person on
        // the same domain should land on the SAME company id.
        let persons = state.persons.as_ref().unwrap();
        persons
            .upsert("acme", &person_fixture("ana", "ana@acme.com"))
            .await
            .unwrap();
        let app = router(state);
        let resp = app
            .oneshot(req_post(
                "/persons/ana/confirm-enrichment",
                json!({ "company_name": "Acme Corp Updated" }),
            ))
            .await
            .unwrap();
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["company"]["domain"], "acme.com");
        // Updated name flowed through.
        assert_eq!(v["result"]["company"]["name"], "Acme Corp Updated");
    }

    #[tokio::test]
    async fn confirm_uses_explicit_domain_override() {
        let (state, _persons) = build_state(true).await;
        // Person's email is corp; operator confirms a
        // different company domain (e.g., parent corp).
        let app = router(state);
        let resp = app
            .oneshot(req_post(
                "/persons/juan/confirm-enrichment",
                json!({
                    "company_name": "Parent Holding",
                    "company_domain": "Parent.io",
                }),
            ))
            .await
            .unwrap();
        let v = body_to_json(resp).await;
        // Domain lower-cased.
        assert_eq!(v["result"]["company"]["domain"], "parent.io");
    }

    #[tokio::test]
    async fn confirm_rejects_company_when_no_company_store() {
        let (state, _persons) = build_state(false).await;
        let app = router(state);
        let resp = app
            .oneshot(req_post(
                "/persons/juan/confirm-enrichment",
                json!({ "company_name": "Acme" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "companies_not_loaded");
    }

    #[tokio::test]
    async fn confirm_returns_404_for_missing_person() {
        let (state, _) = build_state(true).await;
        let app = router(state);
        let resp = app
            .oneshot(req_post(
                "/persons/ghost/confirm-enrichment",
                json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn confirm_ignores_blank_strings() {
        let (state, persons) = build_state(true).await;
        let app = router(state);
        let resp = app
            .oneshot(req_post(
                "/persons/juan/confirm-enrichment",
                json!({
                    "primary_name": "   ",
                    "company_name": "\t\n",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        // Original name preserved.
        assert_eq!(v["result"]["person"]["primary_name"], "Juan G");
        // No company was upserted.
        assert!(v["result"]["company"].is_null());
        // Status still flips to manual — that's the
        // operator-explicit signal.
        let p = persons
            .get("acme", &PersonId("juan".into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.enrichment_status, EnrichmentStatus::Manual);
    }
}

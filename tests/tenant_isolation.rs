//! Cross-tenant isolation suite — release blocker.
//!
//! Spins up two tenants (acme + globex) in the same process,
//! exercises the full read/write surface of LeadStore +
//! identity stores + scraper cache + outbound publisher,
//! asserts zero leakage in either direction.
//!
//! Per the M15 ground rules: cross-tenant data leakage is a
//! release blocker. Every assertion here MUST pass green
//! before nexo-rs-extension-marketing ships v0.1.0.

use std::path::PathBuf;
use std::sync::Arc;

use nexo_marketing::broker::{DispatchInput, OutboundEmail, OutboundPublisher};
use nexo_marketing::compliance::OutboundGate;
use nexo_marketing::lead::{LeadStore, NewLead};
use nexo_marketing::tenant::TenantId;

use nexo_microapp_sdk::enrichment::cache::migrate as migrate_enrichment_cache;
use nexo_microapp_sdk::enrichment::{EnrichmentCache, SqliteEnrichmentCache};
use nexo_microapp_sdk::identity::{
    open_pool, PersonStore, SqlitePersonEmailStore, SqlitePersonStore,
};
use nexo_tool_meta::marketing::{
    EnrichmentStatus, LeadId, LeadState, Person, PersonId, TenantIdRef, SellerId,
};
use sqlx::sqlite::SqlitePoolOptions;

// ── Fixtures ────────────────────────────────────────────────────

async fn fresh_lead_store(tenant: &str) -> Arc<LeadStore> {
    let s = LeadStore::open(PathBuf::from(":memory:"), TenantId::new(tenant).unwrap())
        .await
        .expect("open lead store");
    Arc::new(s)
}

fn lead_input(id: &str) -> NewLead {
    NewLead {
        id: LeadId(id.into()),
        thread_id: format!("thread-{id}"),
        subject: format!("subject-{id}"),
        person_id: PersonId("p".into()),
        seller_id: SellerId("v".into()),
        last_activity_ms: 1_700_000_000_000,
        score: 0,
        why_routed: vec!["fixture".into()],
    }
}

fn person_fixture(id: &str, tenant: &str, email: &str) -> Person {
    Person {
        id: PersonId(id.into()),
        tenant_id: TenantIdRef(tenant.into()),
        primary_name: format!("Person {id}"),
        primary_email: email.into(),
        alt_emails: vec![],
        company_id: None,
        enrichment_status: EnrichmentStatus::Manual,
        enrichment_confidence: 1.0,
        tags: vec![],
        created_at_ms: 0,
        last_seen_at_ms: 0,
    }
}

// ── Assertions ──────────────────────────────────────────────────

#[tokio::test]
async fn assert_1_lead_get_never_returns_other_tenant_row_with_same_id() {
    let acme = fresh_lead_store("acme").await;
    let globex = fresh_lead_store("globex").await;

    // Same id in BOTH tenants — different rows. The store's
    // file-per-tenant boundary means each call only sees its
    // own DB.
    acme.create(lead_input("lead-shared")).await.unwrap();
    globex.create(lead_input("lead-shared")).await.unwrap();

    let acme_row = acme.get(&LeadId("lead-shared".into())).await.unwrap();
    let globex_row = globex.get(&LeadId("lead-shared".into())).await.unwrap();
    assert!(acme_row.is_some());
    assert!(globex_row.is_some());

    // Globex's store does NOT see acme-only ids.
    acme.create(lead_input("acme-only")).await.unwrap();
    let cross = globex.get(&LeadId("acme-only".into())).await.unwrap();
    assert!(cross.is_none(), "globex must not see acme's lead 'acme-only'");
}

#[tokio::test]
async fn assert_2_followup_sweep_is_tenant_scoped() {
    let acme = fresh_lead_store("acme").await;
    let globex = fresh_lead_store("globex").await;

    // Acme has 3 due leads; globex has 0.
    for i in 0..3 {
        let id = format!("acme-{i}");
        acme.create(lead_input(&id)).await.unwrap();
        acme.set_next_check(&LeadId(id), Some(100), false).await.unwrap();
    }

    let globex_due = globex.list_due_for_followup(10_000, 50).await.unwrap();
    let acme_due = acme.list_due_for_followup(10_000, 50).await.unwrap();
    assert_eq!(globex_due.len(), 0, "globex sweep must not see acme leads");
    assert_eq!(acme_due.len(), 3);
}

#[tokio::test]
async fn assert_3_lead_state_count_per_tenant() {
    let acme = fresh_lead_store("acme").await;
    let globex = fresh_lead_store("globex").await;

    acme.create(lead_input("a-1")).await.unwrap();
    acme.create(lead_input("a-2")).await.unwrap();
    globex.create(lead_input("g-1")).await.unwrap();

    assert_eq!(acme.count_by_state(LeadState::Cold).await.unwrap(), 2);
    assert_eq!(globex.count_by_state(LeadState::Cold).await.unwrap(), 1);
}

#[tokio::test]
async fn assert_4_identity_stores_isolate_persons_with_same_id() {
    let pool = open_pool(":memory:").await.unwrap();
    let store = SqlitePersonStore::new(pool);

    // Same person id 'juan' lives in BOTH tenants as
    // distinct rows — different emails + different companies.
    store
        .upsert("acme", &person_fixture("juan", "acme", "juan@acme.com"))
        .await
        .unwrap();
    store
        .upsert("globex", &person_fixture("juan", "globex", "juan@globex.io"))
        .await
        .unwrap();

    let acme_row = store
        .get("acme", &PersonId("juan".into()))
        .await
        .unwrap()
        .unwrap();
    let globex_row = store
        .get("globex", &PersonId("juan".into()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(acme_row.primary_email, "juan@acme.com");
    assert_eq!(globex_row.primary_email, "juan@globex.io");

    // find_by_email — acme tenant must NOT see globex's email.
    let bad_lookup = store
        .find_by_email("acme", "juan@globex.io")
        .await
        .unwrap();
    assert!(bad_lookup.is_none(), "acme must not find globex's email");
}

#[tokio::test]
async fn assert_5_person_email_finder_isolated_per_tenant() {
    let pool = open_pool(":memory:").await.unwrap();
    let store = SqlitePersonEmailStore::new(pool);

    use nexo_microapp_sdk::identity::PersonEmailStore;

    store
        .add(
            "acme",
            &PersonId("juan".into()),
            "shared@gmail.com",
            true,
        )
        .await
        .unwrap();
    store
        .add(
            "globex",
            &PersonId("ana".into()),
            "shared@gmail.com",
            true,
        )
        .await
        .unwrap();

    let acme_owner = store.find_owner("acme", "shared@gmail.com").await.unwrap();
    let globex_owner = store.find_owner("globex", "shared@gmail.com").await.unwrap();
    assert_eq!(acme_owner.unwrap().0, "juan");
    assert_eq!(globex_owner.unwrap().0, "ana");
}

#[tokio::test]
async fn assert_6_scraper_cache_does_not_share_across_tenants() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    migrate_enrichment_cache(&pool).await.unwrap();
    let cache = SqliteEnrichmentCache::new(pool);

    // Acme scraped acme.com → "saas". Globex's lookup for
    // the same domain MUST return None (corporate
    // enrichments don't bleed cross-tenant).
    cache
        .put(
            "acme",
            "acme.com",
            "{\"industry\":\"saas\"}",
            60_000,
            None,
            0,
        )
        .await
        .unwrap();

    let globex_hit = cache.get("globex", "acme.com", 0).await.unwrap();
    assert!(
        globex_hit.is_none(),
        "globex must not hit acme's scraper cache for the same domain"
    );

    let acme_hit = cache.get("acme", "acme.com", 0).await.unwrap();
    assert!(acme_hit.is_some());
}

#[tokio::test]
async fn assert_7_outbound_publisher_idempotency_is_per_publisher() {
    // Each tenant has its own publisher instance. Same
    // (thread_id, draft_id) in tenant A should NOT block
    // dispatch in tenant B.
    let acme = OutboundPublisher::new(
        TenantId::new("acme").unwrap(),
        OutboundGate::with_defaults(),
    );
    let globex = OutboundPublisher::new(
        TenantId::new("globex").unwrap(),
        OutboundGate::with_defaults(),
    );

    let make_input = || DispatchInput {
        thread_id: "t-1".into(),
        draft_id: "d-1".into(),
        seller_smtp_instance: "v-smtp".into(),
        email: OutboundEmail {
            to: vec!["someone@acme.com".into()],
            cc: vec![],
            bcc: vec![],
            subject: "Re".into(),
            body: format!("body {}", uuid::Uuid::new_v4()),
            in_reply_to: None,
            references: vec![],
        },
    };

    // Acme dispatches once → Publish.
    let acme_first = acme.dispatch(make_input());
    assert!(matches!(
        acme_first,
        nexo_marketing::broker::OutboundDispatchResult::Publish { .. }
    ));

    // Globex dispatches the SAME (t-1, d-1) — must Publish too;
    // idempotency is per-publisher (per-tenant).
    let globex_first = globex.dispatch(make_input());
    assert!(matches!(
        globex_first,
        nexo_marketing::broker::OutboundDispatchResult::Publish { .. }
    ));
}

#[tokio::test]
async fn assert_8_lead_store_delete_does_not_touch_other_tenants() {
    let pool = open_pool(":memory:").await.unwrap();
    let store = SqlitePersonStore::new(pool);

    store
        .upsert("acme", &person_fixture("a-1", "acme", "a@acme.com"))
        .await
        .unwrap();
    store
        .upsert("globex", &person_fixture("g-1", "globex", "g@globex.io"))
        .await
        .unwrap();

    // Wipe acme.
    let n = store.delete_by_tenant("acme").await.unwrap();
    assert_eq!(n, 1);

    // Globex untouched.
    let g = store
        .get("globex", &PersonId("g-1".into()))
        .await
        .unwrap();
    assert!(g.is_some(), "delete_by_tenant must not affect other tenants");
}

//! Integration test for the seller-lookup live-reload
//! pipeline (M15.38 + M15.44 follow-up F15).
//!
//! Goal: prove that an operator's `PUT /config/sellers`
//! actually swaps the in-memory seller index under the
//! broker hop's nose — without a process restart. Unit tests
//! cover the classifier (`maybe_notify_lead_created` etc.) +
//! the lookup (`SellerLookup`) in isolation; this file wires
//! both together via the admin axum router so a regression in
//! the `arc_swap` plumbing surfaces here.
//!
//! Same shape for the template-lookup pipeline shipped in
//! M15.44 — second test asserts `PUT /config/notification_templates`
//! flows through.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tempfile::tempdir;
use tower::util::ServiceExt;

use nexo_marketing::admin::{router, AdminState};
use nexo_marketing::lead::LeadStore;
use nexo_marketing::notification::{
    maybe_notify_lead_created, seller_lookup_from_list, template_lookup_from,
    NotificationOutcome,
};
use nexo_marketing::tenant::TenantId;

use nexo_tool_meta::marketing::{
    DomainKind, IntentClass, Lead, LeadId, LeadState, NotificationChannel,
    NotificationTemplates, PersonId, SellerId, SellerNotificationSettings, SentimentBand,
    TenantIdRef,
};

/// Minimal `ParsedInbound` fixture mirroring the broker hop's
/// decoded payload — no real RFC 5322 parse needed for the
/// lookup-pipeline assertion.
fn parsed_fixture() -> nexo_marketing::broker::inbound::ParsedInbound {
    nexo_marketing::broker::inbound::ParsedInbound {
        instance: "default".into(),
        account_id: "acct-1".into(),
        uid: 42,
        from_email: "cliente@empresa.com".into(),
        from_display_name: Some("Cliente".into()),
        to_emails: vec!["pedro@acme.com".into()],
        reply_to: None,
        subject: "Cotización CRM".into(),
        message_id: Some("abc".into()),
        in_reply_to: None,
        references: Vec::new(),
        body_excerpt: "Hola, queremos cotizar".into(),
        thread_id: "th-1".into(),
        from_domain_kind: DomainKind::Corporate,
    }
}

fn lead_fixture(seller: &str) -> Lead {
    Lead {
        id: LeadId("l-1".into()),
        tenant_id: TenantIdRef("acme".into()),
        thread_id: "th-1".into(),
        subject: "Cotización CRM".into(),
        person_id: PersonId("p-1".into()),
        seller_id: SellerId(seller.into()),
        state: LeadState::Cold,
        score: 0,
        sentiment: SentimentBand::Neutral,
        intent: IntentClass::Browsing,
        topic_tags: Vec::new(),
        last_activity_ms: 0,
        next_check_at_ms: None,
        followup_attempts: 0,
        why_routed: Vec::new(),
        operator_notes: None,
    }
}

/// Build an `AdminState` wired with both lookups + a state
/// root + a lead store. Returns the state root + the two
/// lookups so the test body can both PUT through the router
/// AND introspect the post-PUT lookup snapshots directly.
async fn build_state() -> (
    Arc<AdminState>,
    PathBuf,
    nexo_marketing::notification::SellerLookup,
    nexo_marketing::notification::TemplateLookup,
    tempfile::TempDir,
) {
    let tmp = tempdir().unwrap();
    let state_root = tmp.path().to_path_buf();
    std::fs::create_dir_all(state_root.join("marketing").join("acme")).unwrap();

    let store = LeadStore::open(
        PathBuf::from(":memory:"),
        TenantId::new("acme").unwrap(),
    )
    .await
    .unwrap();

    // Boot lookups empty — that's exactly the boot scenario
    // where `PUT /config/sellers` lands first.
    let seller_lookup = seller_lookup_from_list(Vec::new());
    let template_lookup = template_lookup_from(NotificationTemplates::default());

    let state = Arc::new(
        AdminState::new("secret".into())
            .with_store(Arc::new(store))
            .with_state_root(&state_root)
            .with_seller_lookup(seller_lookup.clone())
            .with_template_lookup(template_lookup.clone()),
    );
    (state, state_root, seller_lookup, template_lookup, tmp)
}

fn auth_req(uri: &str, method: &str, body: Value) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .method(method)
        .header(header::AUTHORIZATION, "Bearer secret")
        .header("X-Tenant-Id", "acme")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn body_to_json(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn put_sellers_swaps_lookup_picked_up_by_classifier() {
    // Arrange: empty boot lookup. Classifier should short-
    // circuit `SellerMissing` because nothing's there yet.
    let (state, _root, seller_lookup, _templates, _tmp) = build_state().await;
    let pre = maybe_notify_lead_created(
        &TenantId::new("acme").unwrap(),
        &seller_lookup,
        None,
        &lead_fixture("pedro"),
        &parsed_fixture(),
    );
    assert_eq!(pre, NotificationOutcome::SellerMissing);

    // Act: PUT through the admin router with a single seller
    // that opts into notifications. The handler atomically
    // writes sellers.yaml AND swaps the in-memory lookup.
    let app = router(state);
    let payload = json!({
        "sellers": [{
            "id": "pedro",
            "tenant_id": "acme",
            "name": "Pedro García",
            "primary_email": "pedro@acme.com",
            "alt_emails": [],
            "signature_text": "—\nPedro",
            "on_vacation": false,
            "agent_id": "pedro-agent",
            "notification_settings": {
                "on_lead_created": true,
                "channel": { "kind": "whatsapp", "instance": "personal" }
            }
        }]
    });
    let resp = app
        .oneshot(auth_req("/config/sellers", "PUT", payload))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_to_json(resp).await;
    assert_eq!(v["ok"], true);
    assert_eq!(v["result"]["count"], 1);

    // Assert: the SAME lookup the classifier reads now resolves
    // pedro. No process restart, no manual reload — pure
    // arc_swap living in the admin handler.
    let post = maybe_notify_lead_created(
        &TenantId::new("acme").unwrap(),
        &seller_lookup,
        None,
        &lead_fixture("pedro"),
        &parsed_fixture(),
    );
    match post {
        NotificationOutcome::Publish { topic, payload } => {
            assert_eq!(topic, "agent.email.notification.pedro-agent");
            assert_eq!(payload.seller_id.0, "pedro");
            // Default notification settings → Whatsapp channel
            // with the operator-supplied instance baked in.
            match payload.channel {
                NotificationChannel::Whatsapp { instance } => {
                    assert_eq!(instance, "personal");
                }
                other => panic!("expected Whatsapp, got {other:?}"),
            }
        }
        other => panic!("expected Publish post-PUT, got {other:?}"),
    }
}

#[tokio::test]
async fn put_sellers_then_remove_seller_drops_classifier_match() {
    // Validates the reverse direction: when the operator
    // removes a seller from the YAML, the in-memory lookup
    // shrinks atomically + the next classifier call short-
    // circuits `SellerMissing`.
    let (state, _root, seller_lookup, _templates, _tmp) = build_state().await;
    let app = router(state.clone());

    // PUT pedro first.
    let payload_1 = json!({
        "sellers": [{
            "id": "pedro",
            "tenant_id": "acme",
            "name": "Pedro García",
            "primary_email": "pedro@acme.com",
            "alt_emails": [],
            "signature_text": "—\nPedro",
            "on_vacation": false,
        }]
    });
    let resp = app
        .clone()
        .oneshot(auth_req("/config/sellers", "PUT", payload_1))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Sanity — pedro is in the lookup.
    assert!(seller_lookup
        .load()
        .contains_key(&SellerId("pedro".into())));

    // PUT empty list — pedro evicted.
    let payload_2 = json!({ "sellers": [] });
    let resp = app
        .oneshot(auth_req("/config/sellers", "PUT", payload_2))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(seller_lookup.load().is_empty(), "pedro should be evicted");

    // Classifier sees nothing.
    let out = maybe_notify_lead_created(
        &TenantId::new("acme").unwrap(),
        &seller_lookup,
        None,
        &lead_fixture("pedro"),
        &parsed_fixture(),
    );
    assert_eq!(out, NotificationOutcome::SellerMissing);
}

#[tokio::test]
async fn put_notification_templates_swaps_lookup_picked_up_by_renderer() {
    // M15.44 — same arc_swap pattern for the operator-supplied
    // template overrides. PUT through the admin router; the
    // classifier's render_summary reads from the swapped
    // lookup for the next call.
    let (state, _root, seller_lookup, template_lookup, _tmp) = build_state().await;
    let app = router(state);

    // Seed pedro so we have something to render notifications for.
    let put_seller = json!({
        "sellers": [{
            "id": "pedro",
            "tenant_id": "acme",
            "name": "Pedro García",
            "primary_email": "pedro@acme.com",
            "alt_emails": [],
            "signature_text": "—\nPedro",
            "on_vacation": false,
            "agent_id": "pedro-agent",
            "notification_settings": {
                "on_lead_created": true,
                "channel": { "kind": "whatsapp", "instance": "personal" }
            }
        }]
    });
    let resp = app
        .clone()
        .oneshot(auth_req("/config/sellers", "PUT", put_seller))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Pre-template render — uses the framework's hardcoded
    // ES default ("📧 Nuevo lead de …").
    let pre = maybe_notify_lead_created(
        &TenantId::new("acme").unwrap(),
        &seller_lookup,
        Some(&template_lookup),
        &lead_fixture("pedro"),
        &parsed_fixture(),
    );
    let pre_summary = match pre {
        NotificationOutcome::Publish { payload, .. } => payload.summary,
        other => panic!("expected Publish pre-template, got {other:?}"),
    };
    assert!(pre_summary.starts_with("📧 Nuevo lead"));

    // PUT operator template override — Acme branding.
    let put_templates = json!({
        "templates": {
            "lead_created": {
                "es": "🚀 [Acme Sales] Lead caliente: {{from}}\nAsunto: {{subject}}",
                "en": "🚀 [Acme Sales] Hot lead: {{from}}\nSubject: {{subject}}"
            }
        }
    });
    let resp = app
        .oneshot(auth_req(
            "/config/notification_templates",
            "PUT",
            put_templates,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_to_json(resp).await;
    assert_eq!(v["result"]["reloaded"], true);

    // Post-template render — the operator's branding wins.
    let post = maybe_notify_lead_created(
        &TenantId::new("acme").unwrap(),
        &seller_lookup,
        Some(&template_lookup),
        &lead_fixture("pedro"),
        &parsed_fixture(),
    );
    match post {
        NotificationOutcome::Publish { payload, .. } => {
            assert!(
                payload.summary.contains("[Acme Sales]"),
                "template should win — got: {}",
                payload.summary
            );
            assert!(payload.summary.contains("Cliente")); // {{from}} resolved
            assert!(payload.summary.contains("Cotización CRM")); // {{subject}}
        }
        other => panic!("expected Publish post-template, got {other:?}"),
    }
}

#[tokio::test]
async fn put_sellers_persists_yaml_to_disk_for_post_restart_reload() {
    // Defense-in-depth — the live-reload pipeline doesn't help
    // if the YAML write itself is broken. Verify the file
    // lands on disk + the load_sellers reader (used at boot)
    // returns the same row.
    let (state, root, _lookup, _templates, _tmp) = build_state().await;
    let app = router(state);
    let payload = json!({
        "sellers": [{
            "id": "ana",
            "tenant_id": "acme",
            "name": "Ana Pérez",
            "primary_email": "ana@acme.com",
            "alt_emails": ["ana.perez@acme.com"],
            "signature_text": "—\nAna",
            "on_vacation": false,
        }]
    });
    let resp = app
        .oneshot(auth_req("/config/sellers", "PUT", payload))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Read the YAML directly via the boot loader — same code
    // path the extension hits at startup.
    let loaded = nexo_marketing::config::load_sellers(
        &root,
        &TenantId::new("acme").unwrap(),
    )
    .unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id.0, "ana");
    assert_eq!(loaded[0].alt_emails, vec!["ana.perez@acme.com".to_string()]);
}

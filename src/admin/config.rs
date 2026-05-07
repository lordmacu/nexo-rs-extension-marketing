//! `GET /config/*` — read-only YAML config endpoints.
//!
//! Each handler validates the auth-stamped tenant, resolves
//! the state root from `AdminState`, and delegates to a
//! `crate::config::load_*` helper. Missing config file → empty
//! list (operator hasn't configured the entity yet); parse
//! failure → 500 with the typed error body.
//!
//! Write endpoints (`PUT`) are intentionally not exposed at
//! this milestone — operators still hand-edit YAML; the GET
//! surface unblocks the agent-creator microapp's Settings
//! tabs (mailboxes / vendedores / rules / followup_profiles)
//! to render real data instead of mock fixtures.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nexo_tool_meta::marketing::{FollowupProfile, MailboxConfig, RuleSet, Vendedor};
use serde_json::{json, Value};

use super::AdminState;
use crate::config::{
    load_followup_profiles, load_mailboxes, load_vendedores, save_followup_profiles,
    save_mailboxes, save_rules, save_vendedores,
};
use crate::error::MarketingError;
use crate::lead::router::load_rule_set;
use crate::lead::LeadRouter;
use crate::tenant::TenantId;

/// `GET /config/mailboxes` — list of `MailboxConfig` rows from
/// `mailboxes.yaml`. Empty list when missing.
pub async fn list_mailboxes(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
) -> Response {
    let root = match &state.state_root {
        Some(r) => r,
        None => return state_root_missing(),
    };
    match load_mailboxes(root, &tenant_id) {
        Ok(rows) => ok(json!({ "mailboxes": rows, "count": rows.len() })),
        Err(e) => marketing_error(e),
    }
}

/// `GET /config/vendedores` — list of `Vendedor` rows from
/// `vendedores.yaml`. Empty list when missing.
pub async fn list_vendedores(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
) -> Response {
    let root = match &state.state_root {
        Some(r) => r,
        None => return state_root_missing(),
    };
    match load_vendedores(root, &tenant_id) {
        Ok(rows) => ok(json!({ "vendedores": rows, "count": rows.len() })),
        Err(e) => marketing_error(e),
    }
}

/// `GET /config/rules` — full `RuleSet` (rules + default
/// target + version). Distinct shape from the list endpoints
/// because rules are a single document, not a list of rows.
pub async fn get_rules(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
) -> Response {
    let root = match &state.state_root {
        Some(r) => r,
        None => return state_root_missing(),
    };
    match load_rule_set(root, &tenant_id) {
        Ok(rule_set) => ok(json!({ "rule_set": rule_set })),
        Err(e) => marketing_error(e),
    }
}

/// `GET /config/followup_profiles` — list of `FollowupProfile`
/// cadences from `followup_profiles.yaml`. Empty list when
/// missing.
pub async fn list_followup_profiles(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
) -> Response {
    let root = match &state.state_root {
        Some(r) => r,
        None => return state_root_missing(),
    };
    match load_followup_profiles(root, &tenant_id) {
        Ok(rows) => ok(json!({ "profiles": rows, "count": rows.len() })),
        Err(e) => marketing_error(e),
    }
}

// ── Write endpoints ─────────────────────────────────────────────
//
// Body shape: `{ "<key>": [...] }` where `<key>` matches the GET
// envelope for symmetry — operator UI POSTs the same shape it
// receives. Validation: deserialise the typed `Vec<T>` directly
// from the body, so anything unparseable is `400 invalid_payload`
// before the YAML write starts. Atomic write via `save_yaml_atomic`.

/// Generic body parser: pull `key` out of the JSON envelope and
/// deserialise it as `Vec<T>`. Returns the typed list or a 400
/// response with a typed error code.
fn extract_list<T: serde::de::DeserializeOwned>(
    body: &Value,
    key: &str,
) -> Result<Vec<T>, Response> {
    let inner = body.get(key).ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            "missing_field",
            &format!("body must carry a '{key}' field"),
        )
    })?;
    serde_json::from_value::<Vec<T>>(inner.clone()).map_err(|e| {
        error(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            &format!("'{key}' failed validation: {e}"),
        )
    })
}

/// `PUT /config/mailboxes` — full-replace write. Body:
/// `{ "mailboxes": [<MailboxConfig>...] }`. Returns the parsed
/// list back so the operator UI can re-seed its store from the
/// same shape it would have fetched.
pub async fn put_mailboxes(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<Value>,
) -> Response {
    let root = match &state.state_root {
        Some(r) => r,
        None => return state_root_missing(),
    };
    let rows: Vec<MailboxConfig> = match extract_list(&body, "mailboxes") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if let Err(e) = save_mailboxes(root, &tenant_id, &rows) {
        return marketing_error(e);
    }
    ok(json!({ "mailboxes": rows, "count": rows.len() }))
}

/// `PUT /config/vendedores`. Body: `{ "vendedores": [...] }`.
pub async fn put_vendedores(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<Value>,
) -> Response {
    let root = match &state.state_root {
        Some(r) => r,
        None => return state_root_missing(),
    };
    let rows: Vec<Vendedor> = match extract_list(&body, "vendedores") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if let Err(e) = save_vendedores(root, &tenant_id, &rows) {
        return marketing_error(e);
    }
    // M15.38 — live-reload the vendedor lookup so the broker
    // hop's next notification publish reads the fresh
    // `notification_settings`. Same `arc_swap` pattern as
    // `put_rules` (M15.33). When the handle isn't wired
    // (legacy embedders) the save still lands but
    // notifications use stale settings until restart.
    if let Some(handle) = &state.vendedor_lookup {
        let map: std::collections::HashMap<_, _> =
            rows.iter().map(|v| (v.id.clone(), v.clone())).collect();
        handle.store(std::sync::Arc::new(map));
        tracing::debug!(
            target: "extension.marketing.config",
            tenant = %tenant_id.as_str(),
            count = rows.len(),
            "vendedor lookup live-reloaded after PUT /config/vendedores"
        );
    }
    ok(json!({ "vendedores": rows, "count": rows.len() }))
}

/// `PUT /config/followup_profiles`. Body: `{ "profiles": [...] }`.
pub async fn put_followup_profiles(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<Value>,
) -> Response {
    let root = match &state.state_root {
        Some(r) => r,
        None => return state_root_missing(),
    };
    let rows: Vec<FollowupProfile> = match extract_list(&body, "profiles") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if let Err(e) = save_followup_profiles(root, &tenant_id, &rows) {
        return marketing_error(e);
    }
    ok(json!({ "profiles": rows, "count": rows.len() }))
}

/// `PUT /config/rules` — single-document write. Body:
/// `{ "rule_set": { ...RuleSet... } }`. The router DOESN'T
/// auto-reload from disk yet — caller-side acknowledgement
/// payload includes a `restart_required: true` flag so the
/// operator UI can surface a banner. Live reload arrives in
/// M15.33 once the file watcher + atomic-swap pipeline lands.
pub async fn put_rules(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<Value>,
) -> Response {
    let root = match &state.state_root {
        Some(r) => r,
        None => return state_root_missing(),
    };
    let rule_set_value = match body.get("rule_set") {
        Some(v) => v.clone(),
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "missing_field",
                "body must carry a 'rule_set' field",
            );
        }
    };
    let rule_set: RuleSet = match serde_json::from_value(rule_set_value) {
        Ok(v) => v,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_payload",
                &format!("rule_set failed validation: {e}"),
            );
        }
    };
    // Defense-in-depth: refuse cross-tenant writes. The body's
    // tenant_id MUST match the auth-stamped tenant.
    if rule_set.tenant_id.0 != tenant_id.as_str() {
        return error(
            StatusCode::FORBIDDEN,
            "tenant_mismatch",
            &format!(
                "rule_set.tenant_id {:?} does not match auth-stamped tenant {:?}",
                rule_set.tenant_id.0,
                tenant_id.as_str()
            ),
        );
    }
    if let Err(e) = save_rules(root, &tenant_id, &rule_set) {
        return marketing_error(e);
    }
    // Live reload: rebuild the router with the freshly written
    // YAML and atomically swap it into the shared handle. The
    // broker hop's next `load_full()` picks up the new rules.
    // When the router handle isn't wired (older deployments,
    // unit tests opting out), surface `restart_required: true`
    // so the operator UI banners the operator into a manual
    // restart — graceful degradation rather than dropping the
    // PUT silently.
    let reloaded = match &state.router {
        Some(handle) => {
            // Re-read from disk so the in-memory router shape
            // matches whatever the loader would produce on a
            // fresh boot — covers any post-write coercion (e.g.
            // missing-field defaults) that `save_rules` might
            // not have re-applied.
            match load_rule_set(root, &tenant_id) {
                Ok(rs) => {
                    let new_router = LeadRouter::new(tenant_id.clone(), rs);
                    handle.store(std::sync::Arc::new(new_router));
                    tracing::info!(
                        target: "extension.marketing.config",
                        tenant = %tenant_id.as_str(),
                        rule_count = rule_set.rules.len(),
                        "router live-reloaded from rules.yaml"
                    );
                    true
                }
                Err(e) => {
                    tracing::error!(
                        target: "extension.marketing.config",
                        tenant = %tenant_id.as_str(),
                        error = %e,
                        "rules.yaml saved but router reload failed; restart required"
                    );
                    false
                }
            }
        }
        None => false,
    };
    ok(json!({
        "rule_set": rule_set,
        "reloaded": reloaded,
        "restart_required": !reloaded,
    }))
}

fn state_root_missing() -> Response {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "config_state_root_not_set",
        "AdminState was built without `with_state_root` — config endpoints disabled",
    )
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

fn marketing_error(e: MarketingError) -> Response {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "config_load_failed",
        &e.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request};
    use tempfile::tempdir;
    use tower::util::ServiceExt;

    use crate::admin::router;
    use crate::lead::LeadStore;
    use std::path::PathBuf;

    async fn build_state_with_tenant_dir() -> (Arc<AdminState>, tempfile::TempDir) {
        let tmp = tempdir().unwrap();
        let store = LeadStore::open(
            PathBuf::from(":memory:"),
            TenantId::new("acme").unwrap(),
        )
        .await
        .unwrap();
        let state = Arc::new(
            AdminState::new("secret".into())
                .with_store(Arc::new(store))
                .with_state_root(tmp.path()),
        );
        // Materialise the tenant subdir so config files can
        // land there.
        fs::create_dir_all(tmp.path().join("marketing").join("acme")).unwrap();
        (state, tmp)
    }

    fn req(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("X-Tenant-Id", "acme")
            .body(Body::empty())
            .unwrap()
    }

    async fn body_to_json(resp: Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn missing_files_return_empty_lists() {
        let (state, _tmp) = build_state_with_tenant_dir().await;
        let app = router(state);
        for path in [
            "/config/mailboxes",
            "/config/vendedores",
            "/config/followup_profiles",
        ] {
            let resp = app.clone().oneshot(req(path)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{path}");
            let v = body_to_json(resp).await;
            assert_eq!(v["ok"], true, "{path}");
            assert_eq!(v["result"]["count"], 0, "{path}");
        }
    }

    #[tokio::test]
    async fn rules_returns_default_drop_when_missing() {
        let (state, _tmp) = build_state_with_tenant_dir().await;
        let app = router(state);
        let resp = app.oneshot(req("/config/rules")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        // load_rule_set returns an empty rule set with default
        // target = Drop when the file is missing.
        assert!(v["result"]["rule_set"]["rules"].is_array());
    }

    #[tokio::test]
    async fn vendedores_yaml_renders_via_endpoint() {
        let (state, tmp) = build_state_with_tenant_dir().await;
        let yaml = "\
- id: pedro
  tenant_id: acme
  name: Pedro García
  primary_email: pedro@acme.com
  alt_emails: []
  signature_text: |
    —
    Pedro
  on_vacation: false
";
        fs::write(
            tmp.path().join("marketing").join("acme").join("vendedores.yaml"),
            yaml,
        )
        .unwrap();
        let app = router(state);
        let resp = app.oneshot(req("/config/vendedores")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["count"], 1);
        assert_eq!(v["result"]["vendedores"][0]["id"], "pedro");
        assert_eq!(v["result"]["vendedores"][0]["primary_email"], "pedro@acme.com");
    }

    #[tokio::test]
    async fn parse_error_returns_500_typed_code() {
        let (state, tmp) = build_state_with_tenant_dir().await;
        fs::write(
            tmp.path().join("marketing").join("acme").join("mailboxes.yaml"),
            "this: is: malformed: yaml: oh: no",
        )
        .unwrap();
        let app = router(state);
        let resp = app.oneshot(req("/config/mailboxes")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "config_load_failed");
    }

    #[tokio::test]
    async fn missing_state_root_surfaces_typed_error() {
        // Skip `with_state_root` — endpoints should reject.
        let store = LeadStore::open(
            PathBuf::from(":memory:"),
            TenantId::new("acme").unwrap(),
        )
        .await
        .unwrap();
        let state = Arc::new(
            AdminState::new("secret".into()).with_store(Arc::new(store)),
        );
        let app = router(state);
        let resp = app.oneshot(req("/config/mailboxes")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "config_state_root_not_set");
    }

    // ── PUT (write) endpoint tests ─────────────────────────────

    fn put_req(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method("PUT")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("X-Tenant-Id", "acme")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    #[tokio::test]
    async fn put_vendedores_writes_yaml_and_get_round_trips() {
        let (state, tmp) = build_state_with_tenant_dir().await;
        let payload = json!({
            "vendedores": [{
                "id": "pedro",
                "tenant_id": "acme",
                "name": "Pedro García",
                "primary_email": "pedro@acme.com",
                "alt_emails": [],
                "signature_text": "—\nPedro",
                "on_vacation": false,
            }]
        });
        let app = router(state.clone());
        let resp = app
            .clone()
            .oneshot(put_req("/config/vendedores", payload))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["count"], 1);
        // YAML file should exist on disk + GET should return it.
        let yaml_path = tmp
            .path()
            .join("marketing")
            .join("acme")
            .join("vendedores.yaml");
        assert!(yaml_path.exists());
        let resp = app.oneshot(req("/config/vendedores")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["vendedores"][0]["id"], "pedro");
    }

    #[tokio::test]
    async fn put_missing_field_returns_400_typed() {
        let (state, _tmp) = build_state_with_tenant_dir().await;
        let app = router(state);
        let resp = app
            .oneshot(put_req("/config/mailboxes", json!({"wrong_key": []})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "missing_field");
    }

    #[tokio::test]
    async fn put_invalid_payload_returns_400_typed() {
        let (state, _tmp) = build_state_with_tenant_dir().await;
        let app = router(state);
        let resp = app
            .oneshot(put_req(
                "/config/vendedores",
                json!({ "vendedores": [{ "id": "x" }] }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "invalid_payload");
    }

    #[tokio::test]
    async fn put_rules_without_router_handle_signals_restart_required() {
        // No `with_router` → reload path can't run.
        let (state, _tmp) = build_state_with_tenant_dir().await;
        let payload = json!({
            "rule_set": {
                "tenant_id": "acme",
                "version": 1,
                "rules": [],
                "default_target": { "kind": "drop" },
            }
        });
        let app = router(state);
        let resp = app
            .oneshot(put_req("/config/rules", payload))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["reloaded"], false);
        assert_eq!(v["result"]["restart_required"], true);
    }

    #[tokio::test]
    async fn put_rules_with_router_handle_swaps_live() {
        use crate::lead::{router_handle, LeadRouter};
        use nexo_tool_meta::marketing::{AssignTarget, RuleSet, TenantIdRef};

        // Build state with a router handle wired in. Initial
        // router has the default `Drop` target.
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("marketing").join("acme")).unwrap();
        let store = LeadStore::open(
            PathBuf::from(":memory:"),
            TenantId::new("acme").unwrap(),
        )
        .await
        .unwrap();
        let initial_rule_set = RuleSet {
            tenant_id: TenantIdRef("acme".into()),
            version: 0,
            rules: Vec::new(),
            default_target: AssignTarget::Drop,
        };
        let handle = router_handle(LeadRouter::new(
            TenantId::new("acme").unwrap(),
            initial_rule_set,
        ));
        let state = Arc::new(
            AdminState::new("secret".into())
                .with_store(Arc::new(store))
                .with_state_root(tmp.path())
                .with_router(handle.clone()),
        );

        let app = router(state);
        // PUT a rule set whose default target picks "pedro".
        let payload = json!({
            "rule_set": {
                "tenant_id": "acme",
                "version": 2,
                "rules": [],
                "default_target": { "kind": "vendedor", "id": "pedro" },
            }
        });
        let resp = app
            .oneshot(put_req("/config/rules", payload))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["reloaded"], true, "{v:?}");
        assert_eq!(v["result"]["restart_required"], false);

        // Inspect the swapped router via the handle. Default
        // target should now be Vendedor("pedro"), not Drop.
        let snap = handle.load_full();
        match &snap.rule_set().default_target {
            AssignTarget::Vendedor { id } => assert_eq!(id.0, "pedro"),
            other => panic!("expected Vendedor, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn put_rules_cross_tenant_body_returns_403() {
        // Body claims tenant=globex; auth-stamped tenant=acme.
        // Defense-in-depth — we never trust the body's tenant.
        let (state, _tmp) = build_state_with_tenant_dir().await;
        let payload = json!({
            "rule_set": {
                "tenant_id": "globex",
                "version": 1,
                "rules": [],
                "default_target": { "kind": "drop" },
            }
        });
        let app = router(state);
        let resp = app
            .oneshot(put_req("/config/rules", payload))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "tenant_mismatch");
    }

    #[tokio::test]
    async fn put_followup_profiles_round_trips() {
        let (state, _tmp) = build_state_with_tenant_dir().await;
        let payload = json!({
            "profiles": [{
                "id": "default",
                "cadence": ["24h", "72h"],
                "max_attempts": 2,
                "stop_on_reply": true,
            }]
        });
        let app = router(state);
        let resp = app
            .oneshot(put_req("/config/followup_profiles", payload))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["count"], 1);
        assert_eq!(v["result"]["profiles"][0]["id"], "default");
    }
}

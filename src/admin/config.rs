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
use serde_json::json;

use super::AdminState;
use crate::config::{load_followup_profiles, load_mailboxes, load_vendedores};
use crate::error::MarketingError;
use crate::lead::router::load_rule_set;
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
}

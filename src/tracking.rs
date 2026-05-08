//! Engagement tracking glue (M15.23.a.3).
//!
//! Wraps the SDK's `tracking` module (token signer + HTML
//! helpers + sqlite store) into the per-tenant deps the
//! marketing extension carries on its admin server, and
//! exposes a high-level `prepare_outbound_email` helper that
//! the outbound pipeline calls before a draft is published
//! to `plugin.outbound.email.<instance>`.
//!
//! The routes themselves live in `crate::admin::tracking` —
//! this file only owns the deps + outbound prep.

use std::sync::Arc;

use chrono::Utc;
use nexo_microapp_sdk::tracking::{
    inject_pixel, rewrite_links, MsgId, TrackingStore, TrackingStoreError,
    TrackingTokenSigner,
};
use uuid::Uuid;

use crate::tenant::TenantId;

/// Per-extension tracking deps. Constructed at boot from the
/// operator's secrets file + `state_root` SQLite path; threaded
/// through the binary as `Arc<TrackingDeps>` so the outbound
/// publisher and the ingest routes share the exact same signer
/// + store instance.
pub struct TrackingDeps {
    /// HMAC signer/verifier for pixel + click URL tags.
    pub signer: TrackingTokenSigner,
    /// Async SQLite-backed store (links + open/click events).
    pub store: Arc<dyn TrackingStore>,
    /// Public base URL the operator's reverse proxy maps onto
    /// the marketing extension's HTTP port (e.g.
    /// `https://track.acme.com`). Pixel + redirector URLs are
    /// composed against this.
    pub base_url: String,
}

impl TrackingDeps {
    /// Wire a fresh deps bundle. The caller owns the lifecycle
    /// of the underlying SQLite pool (`store`) so it can share
    /// the same connection between sibling stores.
    pub fn new(
        signer: TrackingTokenSigner,
        store: Arc<dyn TrackingStore>,
        base_url: String,
    ) -> Self {
        Self {
            signer,
            store,
            base_url,
        }
    }
}

/// Reasons the outbound prep can fail. The caller maps each
/// to a different audit log so a transient store hiccup
/// doesn't get confused with a programmer error.
#[derive(Debug, thiserror::Error)]
pub enum TrackingPrepError {
    /// Underlying SQLite error while persisting the rewritten
    /// link map.
    #[error("tracking store: {0}")]
    Store(#[from] TrackingStoreError),
}

/// Run the open-pixel + link-rewrite pipeline on an outbound
/// HTML body. Mutates `html_body` in place and persists every
/// rewritten anchor's `(tenant, msg, link) → original_url`
/// mapping to the store so the click redirector can resolve.
///
/// `msg_id` is generated fresh per call (UUIDv4) and returned
/// so the caller can stamp it on the audit row + the
/// `Message-Id:` header.
///
/// Idempotency: callers MUST NOT invoke this twice on the same
/// body — the second call would inject a second pixel and
/// re-rewrite the (already-redirected) anchors recursively.
/// The redirector-prefix guard in `rewrite_links` catches the
/// most common slip but tracking is conceptually a per-send
/// operation; the operator's outbound publisher already gates
/// on `(thread_id, draft_id)` idempotency so a retry sees the
/// `Skipped` short-circuit before reaching this fn.
pub async fn prepare_outbound_email(
    deps: &TrackingDeps,
    tenant_id: &TenantId,
    html_body: &mut String,
) -> Result<MsgId, TrackingPrepError> {
    let msg_id = MsgId::new(Uuid::new_v4().to_string());
    let now_ms = Utc::now().timestamp_millis();

    // 1. Rewrite anchors → click redirector URLs. Capture the
    //    mappings before touching the body so a store error
    //    leaves the original `html_body` intact.
    let outcome = rewrite_links(
        html_body,
        &deps.base_url,
        tenant_id.as_str(),
        &msg_id,
        &deps.signer,
    );
    for mapping in &outcome.mappings {
        deps.store
            .register_link(
                tenant_id.as_str(),
                &msg_id,
                &mapping.link_id,
                &mapping.original_url,
                now_ms,
            )
            .await?;
    }

    // 2. Inject the open-pixel right before `</body>`. Pixel
    //    URL carries the same HMAC signer the verifier uses on
    //    ingest — forged pixel hits 401 on `/t/o`.
    let pixel_token = deps.signer.sign_open(tenant_id.as_str(), &msg_id);
    let pixel_url = format!(
        "{base}/t/o/{tenant}/{msg}?tag={tag}",
        base = deps.base_url.trim_end_matches('/'),
        tenant = url_path_escape(tenant_id.as_str()),
        msg = url_path_escape(msg_id.as_str()),
        tag = pixel_token.as_str(),
    );

    *html_body = inject_pixel(&outcome.html, &pixel_url);

    Ok(msg_id)
}

fn url_path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if matches!(
            b,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~'
        ) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexo_microapp_sdk::tracking::{open_pool, LinkId, SqliteTrackingStore};

    async fn fresh_deps() -> TrackingDeps {
        let pool = open_pool(":memory:").await.unwrap();
        let store: Arc<dyn TrackingStore> =
            Arc::new(SqliteTrackingStore::new(pool));
        let signer =
            TrackingTokenSigner::new(vec![0u8; 32]).unwrap();
        TrackingDeps::new(signer, store, "https://t.example".into())
    }

    #[tokio::test]
    async fn prepare_injects_pixel_and_rewrites_link() {
        let deps = fresh_deps().await;
        let tenant = TenantId::new("acme").unwrap();
        let mut body = String::from(
            r#"<html><body>Hi <a href="https://acme.com/x">click</a></body></html>"#,
        );
        let msg_id = prepare_outbound_email(&deps, &tenant, &mut body)
            .await
            .unwrap();

        // Pixel injected.
        assert!(body.contains("<img"));
        assert!(body.contains(&format!("/t/o/acme/{msg_id}")));
        // Anchor rewritten.
        assert!(body.contains(&format!("/t/c/acme/{msg_id}/L0")));
        assert!(!body.contains("https://acme.com/x"));
        // Mapping persisted.
        let resolved = deps
            .store
            .lookup_link(tenant.as_str(), &msg_id, &LinkId::new("L0"))
            .await
            .unwrap();
        assert_eq!(resolved.as_deref(), Some("https://acme.com/x"));
    }

    #[tokio::test]
    async fn prepare_handles_body_with_no_anchors() {
        let deps = fresh_deps().await;
        let tenant = TenantId::new("acme").unwrap();
        let mut body = String::from("<html><body>hi</body></html>");
        let _ = prepare_outbound_email(&deps, &tenant, &mut body)
            .await
            .unwrap();
        // Pixel still injected.
        assert!(body.contains("<img"));
        // No L0 row.
        let none = deps
            .store
            .lookup_link("acme", &MsgId::new("xxx"), &LinkId::new("L0"))
            .await
            .unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn prepare_returns_distinct_msg_ids() {
        let deps = fresh_deps().await;
        let tenant = TenantId::new("acme").unwrap();
        let mut a = String::from("<body>x</body>");
        let mut b = String::from("<body>y</body>");
        let id_a = prepare_outbound_email(&deps, &tenant, &mut a).await.unwrap();
        let id_b = prepare_outbound_email(&deps, &tenant, &mut b).await.unwrap();
        assert_ne!(id_a, id_b);
    }

    #[tokio::test]
    async fn prepare_is_tenant_scoped() {
        let deps = fresh_deps().await;
        let acme = TenantId::new("acme").unwrap();
        let globex = TenantId::new("globex").unwrap();
        let mut body_a =
            String::from(r#"<body><a href="https://acme.com/x">go</a></body>"#);
        let id_a =
            prepare_outbound_email(&deps, &acme, &mut body_a).await.unwrap();
        // Tenant B can't resolve tenant A's link.
        let cross = deps
            .store
            .lookup_link(globex.as_str(), &id_a, &LinkId::new("L0"))
            .await
            .unwrap();
        assert!(cross.is_none());
    }
}

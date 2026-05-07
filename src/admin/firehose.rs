//! `GET /firehose` — Server-Sent Events stream of lead lifecycle
//! events for the authenticated tenant.
//!
//! Auth: same bearer + `X-Tenant-Id` middleware as every other
//! admin route. The stamped `TenantId` (set by `bearer_and_tenant_middleware`)
//! drives the per-connection filter — operators only see frames
//! for their own tenant; cross-tenant peeking is impossible by
//! construction.
//!
//! Stream format: standard SSE with `event: lead` frames carrying
//! the JSON-serialised [`LeadFirehoseEvent`]. Lagged subscribers
//! receive an `event: lagged` frame with `{"dropped":<n>}` so the
//! UI knows to reconcile via REST.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::response::sse::{Event, Sse};
use futures::Stream;

use nexo_microapp_http::sse::{sse_filtered_broadcast, LaggedBehavior};

use crate::admin::AdminState;
use crate::tenant::TenantId;

/// SSE handler for `/firehose`. Returns a long-lived
/// `Sse<impl Stream>` driven by the per-extension `LeadEventBus`.
/// Filters by the auth-stamped `TenantId` so cross-tenant
/// peeking is blocked at the producer side.
pub async fn handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant): Extension<TenantId>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.firehose.subscribe();
    let tenant_str: String = tenant.as_str().into();
    tracing::info!(
        target: "extension.marketing.firehose",
        tenant = %tenant_str,
        subscribers_after = state.firehose.receiver_count(),
        "firehose: subscriber attached"
    );
    sse_filtered_broadcast(
        rx,
        Some("lead"),
        // Per-connection tenant filter — frames published for
        // other tenants are silently dropped on the producer
        // side.
        move |event: &crate::firehose::LeadFirehoseEvent| event.tenant_id() == tenant_str,
        // Operator dashboard stays open indefinitely.
        |_| false,
        // Surface lagged via a synthetic frame so the UI can
        // reconcile through REST. The default 256-frame buffer
        // makes this rare in practice.
        LaggedBehavior::Emit {
            event_name: "lagged",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    use crate::admin::router;
    use crate::admin::{AUTH_HEADER, TENANT_HEADER};
    use crate::firehose::{LeadEventBus, LeadFirehoseEvent};
    use crate::lead::LeadStore;
    use nexo_tool_meta::marketing::{LeadId, LeadState, TenantIdRef};

    async fn build_state() -> Arc<AdminState> {
        let store = LeadStore::open(PathBuf::from(":memory:"), TenantId::new("acme").unwrap())
            .await
            .unwrap();
        Arc::new(
            AdminState::new("secret".into())
                .with_store(Arc::new(store))
                .with_firehose(Arc::new(LeadEventBus::new())),
        )
    }

    fn req(uri: &str, bearer: &str, tenant: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(TENANT_HEADER, tenant)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn firehose_requires_bearer() {
        let state = build_state().await;
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/firehose")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Suppress unused import warning when only this test runs.
        let _ = AUTH_HEADER;
    }

    #[tokio::test]
    async fn firehose_rejects_unmounted_tenant() {
        let state = build_state().await;
        let app = router(state);
        let resp = app
            .oneshot(req("/firehose", "secret", "globex"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn firehose_streams_only_matching_tenant() {
        let state = build_state().await;
        let bus = state.firehose.clone();
        let app = router(state);

        // Spawn the request — `Sse` returns a streaming body; we
        // collect the first ~2 KiB to assert what landed without
        // hanging on the keep-alive loop.
        let resp = app
            .oneshot(req("/firehose", "secret", "acme"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );

        // Publish: one event for acme, one for globex. Only acme's
        // should reach the body.
        bus.publish(LeadFirehoseEvent::Created {
            tenant_id: TenantIdRef("acme".into()),
            lead_id: LeadId("l-1".into()),
            thread_id: "th-1".into(),
            subject: "Acme demo".into(),
            from_email: "x@acme.com".into(),
            vendedor_id: "unassigned".into(),
            state: LeadState::Cold,
            at_ms: 1,
            why_routed: vec!["resolver:display_name".into()],
        });
        bus.publish(LeadFirehoseEvent::Created {
            tenant_id: TenantIdRef("globex".into()),
            lead_id: LeadId("l-2".into()),
            thread_id: "th-2".into(),
            subject: "Globex".into(),
            from_email: "x@globex.com".into(),
            vendedor_id: "unassigned".into(),
            state: LeadState::Cold,
            at_ms: 2,
            why_routed: vec![],
        });

        // Read body chunks until we see the `data:` line for
        // acme. Bound the wait so a regression doesn't hang CI.
        let mut body = resp.into_body();
        let mut accumulated = Vec::<u8>::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if tokio::time::Instant::now() > deadline {
                panic!(
                    "did not see acme frame within 2 s — body so far:\n{}",
                    String::from_utf8_lossy(&accumulated)
                );
            }
            match tokio::time::timeout(Duration::from_millis(200), body.frame()).await {
                Ok(Some(Ok(frame))) => {
                    if let Some(chunk) = frame.data_ref() {
                        accumulated.extend_from_slice(chunk);
                    }
                    let s = String::from_utf8_lossy(&accumulated);
                    if s.contains("Acme demo") {
                        // Globex tenant must NOT have leaked through.
                        assert!(
                            !s.contains("Globex"),
                            "tenant filter leaked: globex frame seen by acme subscriber\n{s}"
                        );
                        break;
                    }
                }
                Ok(Some(Err(e))) => panic!("stream error: {e}"),
                Ok(None) => panic!("body closed before acme frame"),
                Err(_) => continue, // tick and re-check
            }
        }
    }
}

//! `GET /healthz` — supervisor probe. Always 200 + a short
//! payload so the daemon's supervisor can read "extension up"
//! without a token. Future M15.24 wires per-mailbox + sqlite
//! status here.
//!
//! Audit fix #8 — also surfaces the broker hop's `in_flight`
//! / `max_in_flight_seen` / `processed_total` counters so the
//! operator can alarm on backpressure without scraping logs.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use super::AdminState;

pub async fn handler(State(state): State<Arc<AdminState>>) -> Json<Value> {
    let metrics = state.broker_metrics.snapshot();
    Json(json!({
        "ok": true,
        "status": "up",
        "version": env!("CARGO_PKG_VERSION"),
        "broker": {
            "in_flight": metrics.in_flight,
            "max_in_flight_seen": metrics.max_in_flight_seen,
            "processed_total": metrics.processed_total,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_ok_with_version_and_broker_metrics() {
        let state = Arc::new(AdminState::new("secret".into()));
        let Json(v) = handler(State(state)).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], "up");
        assert!(v["version"].is_string());
        assert_eq!(v["broker"]["in_flight"], 0);
        assert_eq!(v["broker"]["processed_total"], 0);
    }
}

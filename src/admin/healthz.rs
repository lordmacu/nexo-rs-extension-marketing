//! `GET /healthz` — supervisor probe. Always 200 + a short
//! payload so the daemon's supervisor can read "extension up"
//! without a token. Future M15.24 wires per-mailbox + sqlite
//! status here.
//!
//! **F29 sweep:** generic-shape, 8 LOC of glue. Trivial enough
//! that lifting would add more boilerplate (trait + impl) than
//! it saves.

use axum::Json;
use serde_json::{json, Value};

pub async fn handler() -> Json<Value> {
    Json(json!({
        "ok": true,
        "status": "up",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_ok_with_version() {
        let Json(v) = handler().await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], "up");
        assert!(v["version"].is_string());
    }
}

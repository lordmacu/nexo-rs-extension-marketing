// Subprocess entrypoint. The daemon's plugin-discovery walker
// finds this binary next to its `nexo-plugin.toml` (Phase 81.5)
// and spawns it as a long-lived child speaking JSON-RPC 2.0
// over stdio per `nexo-plugin-contract.md` v1.10.0.
//
// This file stays minimal — feature work lives in `lib.rs` +
// modules so unit tests don't need to spawn the binary.

use std::process::ExitCode;

fn main() -> ExitCode {
    nexo_marketing::init_tracing();
    let version = env!("CARGO_PKG_VERSION");
    tracing::info!(%version, "nexo-marketing starting");
    // Phase 0 of the M15 ejecutar plan: scaffold only. The
    // JSON-RPC stdio loop + handshake + tool registry land in
    // subsequent commits (Bloque E in the plan).
    tracing::info!("scaffold only — handshake loop pending");
    ExitCode::SUCCESS
}

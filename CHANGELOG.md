# Changelog

## 0.1.0 — 2026-05-07 (scaffold)

Initial scaffold for the M15 milestone.

- Cargo bin + lib crate with `nexo-marketing` binary entrypoint.
- Path deps on the framework's `nexo-microapp-sdk`,
  `nexo-tool-meta`, `nexo-compliance-primitives` while the
  extension stabilises; will swap to crates.io versions on
  release.
- Tracing initialiser with `RUST_LOG`-aware filter.
- Skeleton `main` logs a "scaffold only" line + exits;
  handshake loop, tool dispatch, broker subscribers, identity
  resolver, scraper, lead store, routing engine, HTTP admin
  arrive in subsequent commits per the M15 sub-phase plan.

Tracks `agent-creator-microapp/proyecto/PHASES.md` M15.6
(repo scaffold) — the rest of M15 lives in this repo's
subsequent commits.

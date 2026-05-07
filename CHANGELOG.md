# Changelog

## 0.1.0 — 2026-05-07

First end-to-end build of the marketing extension. Subprocess
plugin per the Phase 81.5 contract; admin HTTP loopback
consumed by the agent-creator microapp's `/api/marketing/*`
proxy.

### Implemented (M15.A through M15.I)

- **Wire shapes** in framework `nexo-tool-meta::marketing`
  (Phase 82.15) — bit-equivalent across extension Rust,
  microapp Rust, microapp TypeScript.
- **SDK lifts** in `nexo-microapp-sdk` (M15.3 / 15.4 / 15.5):
  - `identity` — Person + PersonEmail + Company stores +
    sqlite default impl, every method tenant-keyed.
  - `routing` — predicate AST + tenant-scoped dispatcher +
    YAML loader.
  - `enrichment` — domain classifier (75+ personal / 30
    disposable providers), TTL cache `(tenant_id, domain)`,
    fallback-chain runner trait.
- **Lead state machine** with validated transitions
  (`cold → engaged → meeting_scheduled → qualified | lost`).
- **Per-tenant sqlite store** at
  `${state_root}/marketing/<tenant_id>/leads.db`. File-per-
  tenant boundary IS the tenant boundary.
- **Identity resolver pipeline** with 5 fallback adapters:
  `display_name`, `signature`, `llm_extractor` (pluggable
  backend trait), `cross_thread`, `reply_to`.
- **Web scraper** for corporate domains: meta tags + JSON-LD
  Organization, robots.txt aware, Semaphore(4) bound.
- **Inbound RFC 5322 decoder** (`mail-parser`-backed) with
  thread-id derivation + disposable-sender drop.
- **Routing dispatcher + heuristic scorer** wrapping the SDK
  primitives.
- **6 tool handlers** advertised in `nexo-plugin.toml`:
  `marketing_lead_profile`, `marketing_lead_route`,
  `marketing_lead_schedule_followup`,
  `marketing_lead_mark_qualified`,
  `marketing_lead_detect_meeting_intent`,
  `marketing_lead_followup_sweep`.
- **Compliance gate** composing anti-loop + opt-out + PII
  redactor + per-recipient rate limit; outbound publisher
  with `(thread_id, draft_id)` idempotency.
- **HTTP admin** axum router on loopback with bearer +
  `X-Tenant-Id` middleware. Routes today: `/healthz`,
  `/leads`, `/leads/:id`. CRUD endpoints for rules /
  mailboxes / vendedores follow.
- **Cross-tenant isolation suite** (`tests/tenant_isolation.rs`)
  — 8 release-blocker assertions all green: lead get / sweep
  / count / identity persons / person_emails / scraper cache /
  outbound idempotency / delete cascade.

### Test count

138 unit tests + 8 cross-tenant integration = **146 green**.

### Pending

- CRUD admin endpoints for rules / mailboxes / vendedores /
  followup_profiles (need YAML write helpers — M22).
- SSE firehose (`/firehose` → tenant-scoped
  `agent.lead.transition.*`).
- E2E smoke against a fake IMAP server (`testcontainers`).
- Docs polish + crates.io publish.

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

# Changelog

## 0.2.0 — 2026-05-07 (M15.27 — plugin contract stdio loop)

The extension is no longer HTTP-only. It now drives the canonical
[Phase 81.5 plugin contract](https://github.com/lordmacu/nexo-rs/blob/main/nexo-plugin-contract.md):
the daemon's plugin discovery walker spawns `nexo-marketing` as a
subprocess, hands it `tool.invoke` requests over JSON-RPC stdio,
and routes `plugin.inbound.email.*` broker events into the
inbound decoder.

The HTTP loopback admin (consumed by the agent-creator microapp's
`/api/marketing/*` proxy) keeps running alongside on the same
process — both surfaces share the same per-tenant `LeadStore`.

### Implemented

- New `src/plugin/` module:
  - `tool_defs.rs` — 6 `ToolDef` entries (`marketing_lead_*`)
    with strict JSON Schema for the LLM tool catalogue. Lockstep
    test asserts manifest names == defs == `TOOL_NAMES`.
  - `dispatch.rs` — single dispatch closure routing by
    `tool_name` to the existing `crate::tools::*::handle`
    handlers. Maps `ToolError → ToolInvocationError` per the
    `-33401..-33405` band; future `#[non_exhaustive]` variants
    fall through to `ExecutionFailed`.
  - `broker.rs` — `plugin.inbound.email.*` subscriber. Decodes
    the email-plugin `InboundEvent` payload via a private
    deserialise mirror (avoids depending on the email plugin
    crate), runs `decode_inbound_email`, then either creates a
    cold lead or bumps the existing thread's activity. Full
    resolver → router pipeline lands in M22.
- `src/main.rs` rewritten:
  - Drives `PluginAdapter::new(MANIFEST).declare_tools(...)
    .on_tool(...).on_broker_event(...).run_stdio()`.
  - HTTP admin moved into `tokio::spawn` so the main task owns
    stdio (the daemon kills the subprocess if stdout closes).
  - `PluginDeps` shared struct holds `(tenant_id, lead_store,
    router)`.
- 13 new unit tests:
  - 4 `tool_defs.rs` (count match, name match, schema=object,
    every def requires tenant_id).
  - 4 `dispatch.rs` (unknown→NotFound, invalid args→
    ArgumentInvalid, cross-tenant→inline `ok:false`,
    followup_sweep wires the store).
  - 5 `broker.rs` (off-topic→Skipped, malformed→Malformed,
    cold→LeadCreated, repeat→LeadUpdated, bare topic accepted).

### SDK lift

`nexo_microapp_sdk::BrokerEvent` re-exports `nexo_broker::Event`
so plugin authors don't need to add `nexo-broker` directly. Two
lines in `proyecto/crates/microapp-sdk/src/lib.rs`.

### Test count

138 unit + 8 cross-tenant + 6 outbound + **13 plugin** =
**165 green** (was 152).

### Operator note

No operator action required. The dev-daemon path keeps working
unchanged — the daemon discovers the plugin via the existing
`nexo-plugin.toml` and handshakes over stdio automatically. The
HTTP admin port stays at `${MARKETING_HTTP_PORT}` (default
18766) for the agent-creator UI.

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

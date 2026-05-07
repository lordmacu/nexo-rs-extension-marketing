# Changelog

## 0.6.0 — 2026-05-07 (M15.31 — read-only YAML config endpoints)

The 4 admin Settings tabs in the agent-creator microapp now
render real per-tenant YAML data instead of mock fixtures.
Read-only at this milestone; PUT endpoints + YAML write
helpers land in M15.32.

### Implemented

- New `src/config/mod.rs`:
  - `load_mailboxes` / `load_vendedores` /
    `load_followup_profiles` — generic `load_yaml_list<T>`
    over `<state_root>/marketing/<tenant_id>/<file>.yaml`.
  - Missing file → empty `Vec<T>` (operator hasn't
    configured yet).
  - Parse failures surface as `MarketingError::Config` so
    the admin layer 500s with a typed body.
  - 6 unit tests: missing file, vendedor / mailbox /
    followup round-trips, parse error typed, cross-tenant
    isolation via path.
- New `src/admin/config.rs`:
  - 4 `GET /config/{mailboxes|vendedores|rules|followup_profiles}`
    handlers under the existing bearer + `X-Tenant-Id`
    middleware.
  - `state_root_missing` typed 500 surface so misconfigured
    deployments don't return undefined behaviour.
  - 5 admin tests: missing files → empty lists, rules
    default-drop, vendedores YAML renders, parse error
    surfaces typed code, missing state root → typed 500.
- `AdminState::with_state_root` builder + `state_root: Option<PathBuf>`
  field; `main.rs` calls it with the same root used for the
  lead store + identity DB.
- `Cargo.toml` adds `serde_yaml = "0.9"` (was a transitive
  dep through the SDK; pinning explicitly so the loader is
  self-contained).

### Test count

138 unit + 8 cross-tenant + 6 microapp proxy + 25 plugin /
firehose / admin + 7 thread + **11 config (6 loader + 5
admin)** = **195 green** (was 184).

## 0.5.0 — 2026-05-07 (M15.30 — thread persistence + endpoint)

The lead store now persists thread messages — every inbound
broker hop appends an immutable `(message_id, direction,
from_label, body, at_ms)` row, idempotent on the RFC 5322
Message-Id. New `GET /leads/:lead_id/thread` returns the
chronological message list. The agent-creator microapp's
`LeadDetail` UI swaps its placeholder for the live thread.

### Implemented

- `lead/store.rs`:
  - New `thread_messages` table in the same per-tenant
    `leads.db` migration. `(tenant_id, lead_id, message_id)`
    primary key + `(tenant_id, lead_id, at_ms)` index for the
    chronological list query.
  - `LeadStore::append_thread_message` (idempotent ON CONFLICT
    DO NOTHING) + `LeadStore::list_thread` (ORDER BY at_ms ASC).
  - `ThreadMessage` / `MessageDirection` (`inbound` /
    `outbound` / `draft`) / `DraftStatus` typed surface,
    serde-tagged for the wire shape.
  - 4 unit tests: chronological append/list, idempotent on
    duplicate message_id, empty thread for new lead, draft
    status round-trip.
- `plugin/broker.rs`:
  - Both broker paths (cold-thread create + existing-thread
    bump) now `append_thread_message(...)` with the parsed
    inbound. Helper `inbound_message_from_parsed` falls back
    to a synthetic id when the email lacks a Message-Id.
- `admin/leads.rs`:
  - New `thread_handler` mounted at `GET /leads/:lead_id/thread`.
    404 when the lead doesn't exist; 200 with `{lead_id,
    messages, count}` envelope. Messages serialise as
    `inbound|outbound|draft` strings.
  - 3 admin tests: thread returns chronological, empty thread
    is 200, missing lead is 404.

### Test count

138 unit + 8 cross-tenant + 6 microapp proxy + 25 plugin /
firehose / admin + **7 thread (4 store + 3 admin)** =
**184 green** (was 177).

## 0.4.0 — 2026-05-07 (M15.29 — SSE firehose for lead lifecycle)

The extension now publishes a `/firehose` SSE stream of lead
lifecycle events tagged by tenant. The agent-creator microapp's
operator UI subscribes (next commit) so the inbox refreshes
without polling. Stream uses the existing
`nexo-microapp-http::sse::sse_filtered_broadcast` helper —
zero new framework code.

### Implemented

- New `src/firehose/mod.rs`:
  - `LeadFirehoseEvent` tagged enum with `Created`,
    `ThreadBumped`, `Transitioned` variants. Each frame
    carries `tenant_id` for the SSE filter.
  - `LeadEventBus` wraps `tokio::sync::broadcast::Sender`
    (256-frame buffer) with `publish` + `subscribe` +
    `receiver_count` helpers.
  - 6 unit tests covering empty publish, subscriber receive,
    every-variant tenant accessor, receiver count, lagged path,
    JSON wire shape (`kind` discriminator).
- New `src/admin/firehose.rs`:
  - `GET /firehose` SSE handler bound under the existing
    bearer + `X-Tenant-Id` middleware. Filters every frame by
    the auth-stamped tenant — cross-tenant peeking is
    impossible by construction.
  - `LaggedBehavior::Emit { event_name: "lagged" }` so the UI
    can reconcile via REST when frames overflow the buffer.
  - 3 unit tests: missing bearer → 401, unmounted tenant →
    403, end-to-end stream filters out other-tenant frames.
- `AdminState` gains `firehose: Arc<LeadEventBus>` (default-
  initialised) + `with_firehose` builder. Constructor stays
  source-compat.
- `plugin/broker.rs` publishes:
  - `LeadFirehoseEvent::Created` on cold-thread lead create.
  - `LeadFirehoseEvent::ThreadBumped` on existing-thread
    inbound. Both gated on the optional bus param so tests
    can opt out cheaply.
- `main.rs` shares one Arc<LeadEventBus> between the broker
  closure (producer) and the AdminState (consumer surface).
- `handle_inbound_event` signature gains
  `firehose: Option<&LeadEventBus>` (8 args now).
- 2 broker tests for the publish path: `cold_thread_publishes_
  created_event_to_bus` + `second_inbound_publishes_thread_
  bumped_event`.

### Test count

138 unit + 8 cross-tenant + 6 microapp proxy + **25 plugin /
firehose / admin** = **177 green** (was 166).

### Operator note

`scripts/dev-daemon.sh` — no changes; the SSE endpoint
auto-mounts under the existing admin port. Microapp wiring
(SSE proxy + frontend EventSource subscription) lands next.

## 0.3.0 — 2026-05-07 (M15.28 — resolver + router wired into broker hop)

The broker hop is no longer placeholder-only. Each
`plugin.inbound.email.*` event now drives the full
resolver → router pipeline: the SDK identity stores get a
deterministic Person row, the YAML rule set picks a vendedor,
and the lead lands with a real `why_routed` audit trail.

### Implemented

- `src/plugin/mod.rs`:
  - New `IdentityDeps { persons, person_emails, chain }`
    bundle (3 Arcs).
  - `PluginDeps::with_identity` builder so tests can opt out
    while production deployments wire it eagerly.
- `src/plugin/broker.rs` rewritten:
  - `resolve_person` runs the SDK `FallbackChain` against the
    parsed inbound, extracting `EnrichmentResult.source` for
    audit + mapping it to typed `EnrichmentStatus`
    (`signature` / `display_name` / `reply_to` →
    `SignatureParsed`, `llm_extractor` → `LlmExtracted`,
    `cross_thread` → `CrossLinked`).
  - `route_inbound` calls the YAML dispatcher; `Vendedor`
    outcomes use the picked id, `Drop` aborts with
    `HandledOutcome::DroppedByRule`, `NoTarget` (empty
    round-robin pool) falls back to `unassigned`.
  - `persist_person` upserts via `PersonStore::upsert`
    + `PersonEmailStore::add` so subsequent inbounds from
    the same address cross-thread-link cleanly. Errors
    downgrade to placeholder ids — never lose the inbound.
  - `HandledOutcome` gains `DroppedByRule { rule_id }` +
    `LeadCreated.resolver_source` for unit-test introspection.
- `src/main.rs`:
  - Opens the per-tenant identity SQLite pool at
    `${state_root}/marketing/<tenant>/identity.db`,
    instantiates the 3 stores, builds a chain with
    `DisplayNameParser` + `ReplyToReader` (LLM extractor +
    scraper plug in next milestone), and threads
    `IdentityDeps` through `PluginDeps`.
  - Broker subscriber closure captures `(tenant, store,
    router, identity)` Arcs and calls the new
    `handle_inbound_event` signature.
- 6 broker tests (was 5):
  - off-topic skipped, malformed payload, cold-thread create
    with real person id + person_email link verification,
    drop-rule aborts before lead, second inbound bumps thread,
    placeholder path stays functional when identity / router
    are absent.
- Net **+1 test, 166/166 green**.

### Operator note

A new SQLite file lives at
`${state_root}/marketing/<tenant>/identity.db`. The dev-daemon
flow auto-creates it; existing operators upgrading don't need
to migrate state — the file is created on first inbound.

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

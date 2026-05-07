# Changelog

## 0.10.0 — 2026-05-07 (M15.39 — notification forwarder · cierra end-to-end)

The notification publisher (M15.38) now has a consumer: the
same marketing extension subscribes to its own
`agent.email.notification.*` topic and forwards to
`plugin.outbound.{whatsapp,email}.<instance>`. End-to-end
operator notification works without framework changes — the
forwarder lives in the plugin subprocess + uses topic-prefix
dispatch in the existing broker hop.

### Architecture decision

The plugin subprocess CANNOT call admin RPC, so it cannot
resolve `agent.inbound_bindings` at notification time. The
frontend resolves the WA / email plugin instance at vendedor
save-time and bakes the resolved string into
`vendedor.notification_settings.channel`. The marketing
extension's publisher (M15.38) propagates the channel
verbatim into the `EmailNotification` payload; the forwarder
reads from there. Stale bindings (operator re-pairs WA
post-save) require a vendedor re-save — form surfaces the
warning.

### Implemented

- New `src/forwarder.rs`:
  - `classify_forward(topic, payload)` returns
    `ForwardOutcome { Skipped | Malformed | NoChannel |
    UnresolvedInstance{kind} | Forward{topic, body} }` —
    pure classifier, no IO.
  - `handle_notification_event(topic, payload, broker)`
    drives the outcome through `BrokerSender::publish`.
    Failures log warn — never bubble.
  - `whatsapp_outbound_body` builds a
    `{kind: "send_text", to: "agent:<id>", body, metadata}`
    envelope the wabridge plugin consumes.
  - `email_outbound_body` builds a
    `{kind: "send", from_instance, to, subject, body_text,
    metadata}` envelope the email plugin consumes.
  - 9 unit tests cover off-topic skip, malformed payload,
    happy WA path, empty instance short-circuit, happy
    email path, empty `to` short-circuit, Disabled
    pass-through, dotted agent ids, bare topic.
- `nexo-plugin.toml` adds
  `agent.email.notification.*` to `[plugin.subscriptions].broker_topics`.
- `main.rs` broker closure dispatches by topic prefix:
  - `agent.email.notification.*` →
    `forwarder::handle_notification_event`.
  - `plugin.inbound.email.*` → existing inbound pipeline.
- `notification.rs` test fixtures updated for the new
  channel shape (`Whatsapp { instance }` /
  `Email { from_instance, to }`).
- New `FOLLOWUPS.md` tracks F2-F17 with severity + effort
  estimates.

### Test count

138 unit + 8 cross-tenant + 6 microapp proxy + 25 plugin /
firehose / admin + 7 thread + 23 config + 1 live-reload +
9 notification + **9 forwarder** = **226 green** (was 217).

### End-to-end flow now works

1. Inbound email → broker hop creates lead.
2. `maybe_notify_lead_created` builds `EmailNotification`
   with the baked channel (e.g. `Whatsapp { instance: "personal" }`).
3. `BrokerSender::publish("agent.email.notification.pedro-agent", …)`.
4. NATS broker delivers the event back to the marketing
   extension (same plugin, declared subscription).
5. `forwarder::handle_notification_event` decodes, classifies,
   publishes `plugin.outbound.whatsapp.personal` with the
   summary as `body`.
6. The wabridge plugin (subscribed to its own outbound topic)
   sends the message to the operator's phone.

### Operator setup

The forwarder runs automatically — no extra config beyond
the existing M15.38 vendedor save flow. Frontend (M15.39)
auto-resolves the WA instance from `agent.inbound_bindings`
when the operator picks the `Whatsapp` channel. For email,
operator types the `from_instance` (mailbox id from
`/m/marketing/settings/mailboxes`) + `to`.

## 0.9.0 — 2026-05-07 (M15.38 — operator notifications via agent)

Marketing publishes typed `EmailNotification` frames to
`agent.email.notification.<agent_id>` whenever a lead-
lifecycle event matches the bound vendedor's per-event
toggles. The agent's runtime / sidecar subscribes and
forwards via the configured channel — WhatsApp (default,
reuses agent's existing inbound binding), email (to an
arbitrary address), or disabled (publish without forward).

This commit ships the publisher side: wire shape consumed,
vendedor lookup with `arc_swap` live-reload, broker hop
gates the publish on settings + agent_id presence, classifier
tested across the 6 short-circuit branches.

### Implemented

- `Cargo.toml`: no new deps — `arc-swap` already present
  from M15.33.
- New `src/notification.rs`:
  - `VendedorLookup = Arc<ArcSwap<HashMap<VendedorId, Vendedor>>>`
    + `vendedor_lookup_from_list(rows)` builder.
  - `NotificationOutcome` enum: `VendedorMissing`,
    `NotConfigured`, `EventDisabled`, `NoAgentBound`,
    `ChannelDisabled`, `Publish { topic, payload }`.
  - `maybe_notify_lead_created(tenant, lookup, lead, parsed)`
    classifies inline — pure decision separated from the
    broker.publish IO so unit tests don't need a broker
    mock.
  - `render_summary` composes the operator-facing line
    localised to the vendedor's `preferred_language`
    (Spanish default, English when set).
  - 9 unit tests cover every short-circuit branch + the
    happy path + the en-locale path + email channel
    pass-through + arc_swap live-reload.
- `plugin/mod.rs::PluginDeps`:
  - New `vendedores: Option<VendedorLookup>` field.
  - `with_vendedores(lookup)` builder.
- `plugin/broker.rs::handle_inbound_event`:
  - Signature gains `vendedores: Option<&VendedorLookup>`
    + uses the broker sender (was `_broker`) to publish.
  - On `LeadCreated`, resolves the vendedor via lookup,
    runs `maybe_notify_lead_created`, fire-and-forget
    publish via `BrokerSender::publish` with a typed
    `nexo_microapp_sdk::BrokerEvent`. Failures log
    warn but don't sink the broker hop.
- `admin/mod.rs::AdminState`:
  - New `vendedor_lookup: Option<VendedorLookup>` +
    `with_vendedor_lookup` builder.
- `admin/config.rs::put_vendedores`:
  - After atomic YAML write, rebuilds the `HashMap` from
    the freshly persisted rows + `handle.store(Arc::new)`
    — same pattern as `put_rules` (M15.33).
- `main.rs`:
  - Loads `vendedores.yaml` at boot via
    `config::load_vendedores`, builds the lookup, threads
    it through both `PluginDeps` (broker hop) +
    `AdminState` (PUT live-reload).

### Test count

138 unit + 8 cross-tenant + 6 microapp proxy + 25 plugin /
firehose / admin + 7 thread + 23 config + 1 live-reload +
**9 notification** = **217 green** (was 208).

### Operator note

The agent's runtime today doesn't ship a generic forwarder
that subscribes to `agent.email.notification.<id>` — the
publisher side is wired so the topic is live; operators
can hand-roll a small bridge service or wait for the
framework forwarder (M22+ scope). Empty subscriber list
is fine: marketing's `BrokerSender::publish` succeeds even
with zero consumers (logged at debug, not warn).

## 0.8.1 — 2026-05-07 (M15.35 — Vendedor agent binding wire shape)

Picks up the framework's `nexo-tool-meta::marketing::Vendedor`
lift: `agent_id: Option<String>` + `model_override:
Option<ModelRef>`. Backward compatible — existing
`vendedores.yaml` files without the new fields parse cleanly.

### Implemented

- `src/config/mod.rs::tests::fresh_vendedor` adds
  `agent_id: None, model_override: None` to the test fixture
  so the `Vendedor` constructor stays exhaustive after the
  framework change.

No behaviour change yet — the LLM-call pipeline that consumes
`agent_id` lands in M15.36 (LLM extractor adapter switching
to `BrokerSender::complete_llm` driven by the bound agent's
`ModelRef`).

## 0.8.0 — 2026-05-07 (M15.33 — live router reload)

`PUT /config/rules` no longer requires an extension restart.
The router now lives behind an `arc_swap::ArcSwap<LeadRouter>`
shared between the broker hop (reader) and the admin handler
(writer). After a successful YAML write the handler rebuilds
the router from disk and atomically swaps it into the handle;
the next broker event picks up the new rules without a
process bounce.

`load_full()` is a single atomic Arc bump → ~10 ns per
broker hop, no contention on the read path.

### Implemented

- `Cargo.toml` adds `arc-swap = "1"`.
- `lead/router.rs`:
  - New `RouterHandle = Arc<ArcSwap<LeadRouter>>` typedef.
  - `router_handle(router)` builder function.
- `lead/mod.rs` re-exports `RouterHandle` + `router_handle`.
- `plugin/mod.rs::PluginDeps.router` switches from
  `Arc<LeadRouter>` to `RouterHandle`.
- `plugin/dispatch.rs`: `lead_route::handle` now receives a
  `load_full()` snapshot per call so dispatched tools see
  the same generation the broker hop saw.
- `main.rs`: `router_handle(LeadRouter::new(...))` at boot;
  the broker closure does `router_handle.load_full()` per
  event for a snapshot Arc that survives concurrent stores.
- `admin/mod.rs::AdminState`:
  - New `router: Option<RouterHandle>` field.
  - `with_router(handle)` builder.
  - `main.rs` calls it with the same Arc the broker hop
    captured.
- `admin/config.rs::put_rules`:
  - After atomic YAML write, re-reads the file via
    `load_rule_set` so the in-memory router shape matches
    a fresh-boot view (covers any post-write coercion the
    serde defaults might apply).
  - Builds `LeadRouter::new(...)` + `handle.store(Arc::new(...))`.
  - Response gains `reloaded: true` + flips
    `restart_required` to `false`.
  - When the router handle isn't wired (older deployments,
    test fixtures opting out), surface
    `restart_required: true` + `reloaded: false` so the
    operator UI banners the manual-restart workflow —
    graceful degradation.
- `admin/config.rs` tests:
  - Renamed previous `put_rules_round_trips_with_restart_required_flag`
    → `put_rules_without_router_handle_signals_restart_required`.
  - New `put_rules_with_router_handle_swaps_live`: PUTs a
    rule set whose `default_target = Vendedor("pedro")`,
    asserts the handle's `load_full()` returns the new
    target after the request returns. End-to-end live-swap
    proof.

### Test count

138 unit + 8 cross-tenant + 6 microapp proxy + 25 plugin /
firehose / admin + 7 thread + 23 config + **1 live reload**
= **208 green** (was 207).

### Operator note

After upgrading to 0.8.0, every `PUT /config/rules` returns
`reloaded: true` (not `restart_required: true`). Existing
deployments wired through `main.rs` will pick up the
`with_router` call automatically; custom embedders building
their own `AdminState` need to add `with_router(handle)` to
unlock live reload, otherwise the older `restart_required`
flag continues to surface and the workflow degrades to
manual restart.

## 0.7.0 — 2026-05-07 (M15.32 — config WRITE endpoints)

CRUD loop closes: `PUT /config/{mailboxes|vendedores|rules|
followup_profiles}` accept the typed list / single document,
validate via serde, and atomically replace the per-tenant
YAML file on disk.

Atomic write: serialise → write to `.<file>.tmp` in the same
directory → fsync → rename. Same-fs rename is a single inode
swap; a crash mid-write leaves either the old file intact
OR the new file complete, never half-written.

Reload semantics: rules.yaml drives a `LeadRouter` instantiated
at boot; the router doesn't re-read the file yet. PUT
`/config/rules` returns `restart_required: true` so the
operator UI can surface a banner. Live reload (file watcher
+ atomic-swap of the Arc) lands in M15.33.

### Implemented

- `src/config/mod.rs`:
  - `write_yaml_atomic(path, value)` — temp-file +
    fsync + rename helper. Creates the per-tenant subdir
    (and `state_root`) if missing so first-save doesn't
    fail.
  - `save_mailboxes` / `save_vendedores` / `save_followup_profiles`
    over the generic `save_yaml_list` helper.
  - `save_rules` for the single-document RuleSet shape.
  - 6 new write tests: round-trips for the 3 list configs,
    `mkdir -p` on missing dir, atomic-overwrite leaves no
    `.<file>.tmp` stragglers, rules round-trip via direct
    file inspection.
- `src/admin/config.rs`:
  - `extract_list<T>` helper — pull `key` out of the JSON
    envelope + deserialise as `Vec<T>` (or 400 with
    `invalid_payload` / `missing_field`).
  - 4 PUT handlers: `put_mailboxes`, `put_vendedores`,
    `put_followup_profiles`, `put_rules`. Each validates,
    writes, and returns the parsed payload back so the
    operator UI can re-seed without an extra GET.
  - `put_rules` enforces tenant_id matches the auth-stamped
    tenant — defense-in-depth against a misconfigured client
    sending a body with a different tenant id (403
    `tenant_mismatch`).
  - 6 new admin tests: vendedor PUT round-trips through
    GET, missing field 400, invalid payload 400, rules PUT
    flag, cross-tenant 403, followups round-trip.
- `src/admin/mod.rs` mounts each `/config/*` path with both
  GET + PUT via `MethodRouter::put`.

### Test count

138 unit + 8 cross-tenant + 6 microapp proxy + 25 plugin /
firehose / admin + 7 thread + 11 config GET + **12 config
WRITE (6 store + 6 admin)** = **207 green** (was 195).

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

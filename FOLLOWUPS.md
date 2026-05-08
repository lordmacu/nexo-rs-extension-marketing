# Follow-ups · marketing extension

Open follow-ups across the M15 milestones. Triaged by severity.
Each entry: origin milestone · estimated effort · acceptance criteria.

## 🟠 High · UX visible

### F19 · Lead drawer: duplicate-person merge prompt ✅ — done in agent-creator 0.0.64

`DuplicateMergePrompt` component sits under the lead-
drawer header. Mounts on `lead_id` change, fetches
`/audit?lead_id=<id>&kind=duplicate_person_detected&limit=50`
via the new `getAudit()` API client + `/api/marketing/audit`
backend proxy.

`collapseAudit(rows)` (pure helper, exposed for unit
testing) merges multiple audit rows for the same
candidate into one prompt entry — best confidence + union
of signals + earliest `at_ms`. Sorts desc by confidence
so the operator's top suggestion lands first.

UI per candidate: violet banner with `Users` icon +
candidate `person_id` (mono) + confidence chip
(percentage) + signal badges (email_match / phone_match /
name_company_fuzzy) + detail prose row + two buttons:
- **Confirmar merge** disabled with tooltip
  ("Merge endpoint pendiente — operador resuelve
  manualmente"). Real merge mutation lands when the
  endpoint ships (deferred follow-up).
- **Descartar** local-only state (resets on lead change),
  hides the candidate from the prompt without backend
  mutation.

7 vitest unit cases on `collapseAudit`: empty input,
non-duplicate kinds filtered, multi-signal collapse,
detail tracks highest confidence, identical signal
deduped, sort order, earliest at_ms preserved.

86/86 frontend tests green (79 baseline + 7 new). Frontend
0.0.76 → 0.0.77 · agent-creator 0.0.63 → 0.0.64.

### F20 · Lead drawer: engagement badge (M15.23.a.4)

- **Origin:** M15.23.a.4 — `GET /tracking/msg/:msg_id/engagement`
  returns `{ opens, clicks_by_link }` for an outbound
  message. Endpoint shipped + tested but no UI consumes it.
- **Plan:** lead drawer's outbound-message rows render a
  badge ("📧 3 lecturas · 2 clicks") next to the timestamp;
  click expands the per-link breakdown.
- **Effort:** ~150 LOC frontend.
- **Blocker:** real-world `msg_id`s only land when M22's
  draft pipeline + `prepare_outbound_email` integration
  fire. Today's outbound publisher is already wired to call
  the prep helper once a draft consumer exists.

### F2.a · Publish `LeadReplied` ✅ — done in M15.40

### F2.b · `LeadTransitioned` + `MeetingIntent` ✅ — done in M15.41

SDK lift shipped in `nexo-microapp-sdk` Phase 81.17.c.ctx:
new `ToolContext` + `ToolHandlerWithContext` trait +
`PluginAdapter::on_tool_with_context` builder. Marketing
extension migrated `plugin/dispatch.rs` + the 2 affected
tools (`lead_mark_qualified`, `lead_detect_meeting_intent`).
Both publish via `BrokerSender::publish` post-success.

`DraftPending` still deferred to M22 (no draft pipeline yet).

### F4 · Reconciliation race condition ✅ — doc-closed in M15.50

`agent-creator-microapp/README.md` now carries an
"Operational notes" section explaining the
non-transactional `saveSellers` + `agents/upsert` pair, the
single-operator-recommended posture, and the upgrade path
(move reconciliation into a microapp backend route behind a
per-tenant mutex when multi-operator concurrency becomes a
real workload). Code-level fix deferred until that signal
surfaces.

### F7 · Stale `seller.agent_id` when agent deleted ✅ — done in M15.42

Frontend `unbindSellersFromAgent` runs before
`agents/delete`; cascade banner surfaces affected sellers
in the confirm modal. Dropdown filters inactive agents.

### F8 · `LeadReplied` notification ✅ — done in M15.40

## 🟡 Medium · technical debt

### F21 · WhatsApp-side `PersonPhone` ingest ✅ — done in 0.16.0

`crate::whatsapp_ingest::handle_inbound_whatsapp_event`
subscribes to `plugin.inbound.whatsapp.*` (manifest
`broker_topics` extended), decodes the WA plugin's
`InboundEvent::Message` via a private wire-shape
mirror, parses the sender JID through
`nexo_microapp_sdk::identity::parse_jid`, rejects
groups / status / bots via `is_user()`, resolves
tenant through a dedicated `StaticTenantResolver`
(default-falls-back to the operator's
`MARKETING_TENANT_ID`), and upserts `Person`
(deterministic uuid5 from canonical JID) + `PersonPhone`.

9 unit tests via in-memory fake stores: off-topic
skipped, happy-path person+phone persists, deterministic
uuid5 collapses re-ingest, legacy `c.us` canonicalises
to `s.whatsapp.net` (single row across both inbounds),
group JID skipped, non-`message` discriminator filtered,
malformed payload typed-error, LID kept distinct from PN
(F23 covers the bridge), deterministic id function pure +
case-insensitive.

356/356 marketing tests green (347 baseline + 9 wa). The
matcher's phone signal now fires real candidates whenever
an email-side lead arrives carrying a JID a WA contact
already used.

### F22 · Outbound publisher integration of tracking prep helper

- **Origin:** M15.23.a.3 — `prepare_outbound_email(deps,
  tenant, &mut html_body) -> MsgId` is wired + tested but
  the marketing extension's `OutboundPublisher::dispatch`
  never calls it (the AI-draft → outbound send path lands
  in M22).
- **Plan:** when M22 ships the draft pipeline, the
  publisher calls `prepare_outbound_email` between the
  compliance gate and the broker publish. The returned
  `msg_id` lands on the audit log + becomes the
  `Message-Id:` header so engagement events thread back to
  the originating draft.
- **Effort:** ~30 LOC (the heavy lift was the helper
  itself).
- **Blocker:** M22 draft pipeline.

### F23 · `LidPnMapping` store in SDK ✅ — done in SDK 0.1.10 / marketing 0.17.0

`LidPnMappingStore` trait + `SqliteLidPnMappingStore`
default impl in the SDK. Schema: `lid_pn_mappings` table
PK on `(tenant_id, lid_user)` + index on
`(tenant_id, pn_user)` for reverse lookups. Methods:
`put / get_pn_for_lid / get_lid_for_pn / delete_by_tenant`.
First-seen `observed_at_ms` semantics — re-upserting the
same LID with a fresh PN updates the mapping but leaves
the audit stamp intact.

7 SDK unit tests + 5 marketing wa_ingest tests. SDK
332/332 green (324 baseline + 7 lid_pn + 1 migration
counter). Marketing 365/365 green (360 baseline + 5).

`IdentityDeps` gains `lid_pn_mappings: Option<Arc<dyn
LidPnMappingStore>>` + `with_lid_pn_mappings` builder.
`main.rs` boots `SqliteLidPnMappingStore` against the
existing identity pool — no extra DB file.

WA ingest's `handle_inbound_whatsapp_event` consults the
store when the inbound JID is LID-flavoured: a paired PN
hit ⇒ the deterministic person id derives from the PN
canonical form, so both LID + PN inbounds from the same
human collapse onto a single Person row.

`bridge_identity_via_lid_pn_mapping(parsed, store, tenant)`
(private helper) holds the bridge logic. PN inbounds skip
the lookup — they're already the canonical anchor.

Discovery (operator / protocol announces a fresh pair)
deferred to a sub-followup: a future
`plugin.protocol.whatsapp.lid_mapping.*` subscriber or an
admin `POST /api/marketing/lid_pn_mappings` endpoint will
populate rows. Until then the store is consumer-ready —
operators with manual mappings can populate via direct SQL.

Versions: SDK 0.1.9 → 0.1.10 · marketing 0.16.2 → 0.17.0
(minor: new pub IdentityDeps field + new wire shape).

### F24 · Duplicate matcher: broaden name+company search ✅ — done in marketing 0.16.1 / SDK 0.1.9

`PersonStore::list_by_company(tenant, company_id, limit)`
lifted to the SDK trait with a default impl returning empty
(in-tree fakes only override when they want the signal).
SQLite impl runs a tenant-scoped query ordered by
`last_seen_at_ms` desc; `limit` clamped at 1-1000 so a
misconfigured caller can't trigger an unbounded table scan.

`crate::duplicate::find_duplicate_candidates` now seeds its
fuzzy-name comparison pool from
`person_store.list_by_company(tenant, candidate.company_id,
200)` when the candidate has a `company_id`. Falls back to
the previous email/phone-pool comparison when no company id
is set. SQLite errors on `list_by_company` log warn +
degrade to the fallback (matcher remains a hint, never
blocks).

6 SDK tests + 4 marketing tests: matching company rows
returned / null company excluded / tenant scoped / recency
ordering / limit clamping / unknown company empty. Marketing
tests cover: F24 broadening surfaces a candidate even when
no email or phone signal fires, self-match excluded, below-
floor drops, no-company candidate falls back to email pool.

360/360 marketing tests green (356 baseline + 4 F24). 324/324
SDK tests green (318 baseline + 6 list_by_company).
SDK 0.1.8 → 0.1.9 · marketing 0.16.0 → 0.16.1.

### F25 · Cross-restart notification dedup via sled ✅ — done in marketing 0.17.1

`DedupCache` keeps its narrow public surface
(`new` / `with_ttl` / `is_duplicate` / `len`) — internal
`Backend` enum dispatches between the in-memory
`Mutex<HashMap>` (default) and an opt-in sled keyspace.

`DedupCache::with_sled(path, ttl)` opens an embedded
sled DB. Each `is_duplicate` call reads the existing
8-byte LE u64 timestamp; within TTL ⇒ duplicate (don't
refresh — first-seen window stays consistent); past TTL
⇒ overwrite + treated as fresh; missing ⇒ insert + return
false. Sled errors fail open (log warn, return `false`)
so a transient I/O hiccup never blocks the publish.

Boot wiring (`main.rs::build_dedup_cache`):
- Default build / no env: in-memory.
- `--features dedup-sled` AND `MARKETING_DEDUP_SLED=1`
  (or `true`/`yes`/`on`): persistent sled at
  `<state_root>/<tenant>/notification_dedup.sled`.
- Feature compiled but env unset: in-memory (operator
  opts in explicitly).

8 sled-backed tests (all in-memory tests still green via
the same module): first-call records / second-call
duplicates / distinct keys independent / TTL eviction
on stale row / **cross-restart survives reopen** / TTL
eviction across restart / len reports rows / cross-tenant
keys independent. The cross-restart test is the headline
F25 invariant — open → write → drop → reopen → second
call is deduped.

Tests: 365/365 default features · 373/373 with
`dedup-sled` feature (+8 sled). New dep
`sled = "0.34"` is optional.

Bump 0.17.0 → 0.17.1.

### F26 · Sandboxed Handlebars feature ✅ — done in SDK 0.1.12

`nexo_microapp_sdk::templating::handlebars` (gated by
`templating-handlebars`, additive on top of `templating`).
Pulls `handlebars = "5"` with `default-features = false`
so the dep tree stays narrow.

`render_handlebars(template, ctx) -> Result<String,
HandlebarsRenderError>`. Each call builds a fresh
`Handlebars` instance — stateless, thread-safe, no
cross-call leakage.

Sandbox guards (3):
1. **No custom helpers.** Only built-ins (`if` / `else` /
   `unless` / `each` / `with` / `lookup` / `log`); none
   touch fs/network/process state.
2. **No partial loading.** `{{> partial}}` fails as
   `HandlebarsRenderError::Render` because nothing is
   registered. `dev_mode` explicitly OFF.
3. **Lenient missing keys.** `strict_mode` OFF so a typo
   renders an empty string instead of crashing the
   pipeline.

`HandlebarsRenderError` typed enum (`Parse` vs `Render`)
for operator UX — parse errors mean bad syntax, render
errors mean missing helpers / partials.

Convenience wrappers:
- `render_template(&Template, ctx)`
- `render_snippet(&Snippet, ctx)`

17 unit tests cover: simple substitution, `{{#if}}` /
`{{#unless}}` branches, `{{#each}}` with `@index`,
`{{#with}}` scoping, default HTML escape + `{{{raw}}}`
opt-out, missing-key lenient rendering, unregistered
partial rejection, malformed template parse error,
dotted-path resolution, nested if+each composition,
typed `Template`/`Snippet` helpers, empty body, array
`lookup` helper.

Total templating suite: 27/27 (10 mustache-lite +
17 handlebars). SDK 360 → 377.

Caveat: marketing extension does NOT consume yet — the
draft pipeline (M22) is the natural first consumer. The
`Template`/`Snippet` storage already shipped in M15.23.b
will keep both renderers — operators pick which to use
when they author the template. Storage stays format-
agnostic.

Bump SDK 0.1.11 → 0.1.12.

### F6 · Reconciler walk affected agents only ✅ — done in M15.51-2

`reconcileAgentMarketingBindings(sellers, previousSellers?)`
gains an optional second arg. When passed, the reconciler
walks only agents in `(previous.agent_id ∪ next.agent_id)`
instead of every agent in the deployment. Common case:
operator edits one seller → 1 or 2 agents touched regardless
of total agent count. `marketingConfig.ts::saveSellers`
snapshots `slice.data` pre-save and threads it through. 3 new
unit tests cover the fast path + the both-sides binding move
+ backwards-compat omit case.

### F9 · Notification deduplication ✅ — done in M15.53

In-memory `DedupCache` (`src/notification_dedup.rs`,
~145 LOC + 8 unit tests). Key shape
`(tenant, lead_id, kind, at_ms / 60_000)` — minute-bucketed
so genuine 30 s-apart events on the same lead+kind collapse.
1 h TTL, lazy eviction inside `is_duplicate`. `Mutex<HashMap>`
internals; no new deps.

Wired into all 4 publish call sites: broker hop's
`lead_created` + `lead_replied` paths (`src/plugin/broker.rs`)
+ the `lead_mark_qualified` / `lead_detect_meeting_intent`
tools. Single `Arc<DedupCache>` constructed in `main.rs`,
captured by both the broker-event closure + `PluginDeps`
so all 4 surfaces share one TTL window — a NATS redelivery
across surfaces still dedupes.

`sled`-backed cross-restart variant deferred — the in-memory
cache covers ~95 % of the at-least-once redelivery threat
without a new dep. Public surface kept narrow so swap is a
1-file change if cross-restart dedup becomes required.

### F10 · Operator-supplied summary templates ✅ — done in M15.44

`notification_templates.yaml` per tenant, loaded via
`load_notification_templates`, threaded through every
classifier as `Option<&TemplateLookup>`. Render functions try
the operator template first via `nexo-tool-meta::template`'s
`{{path}}` syntax, fall back to hardcoded ES/EN. Live-reload
via `arc_swap` on `PUT /config/notification_templates`.
Frontend Settings tab "Templates" lets operators edit via
JSON editor (M15.34 pattern).

### F15 · Integration test for seller / template live-reload ✅ — done in M15.48

`tests/notification_live_reload.rs` (4 tests, ~280 LOC):
- `put_sellers_swaps_lookup_picked_up_by_classifier` — boot
  empty lookup, PUT through router, classifier sees fresh
  seller without restart.
- `put_sellers_then_remove_seller_drops_classifier_match` —
  reverse direction (PUT empty list evicts seller).
- `put_notification_templates_swaps_lookup_picked_up_by_renderer`
  — same pattern for the M15.44 template lookup; verifies
  `{{from}}` / `{{subject}}` placeholders resolve post-PUT.
- `put_sellers_persists_yaml_to_disk_for_post_restart_reload`
  — defense-in-depth: file landed on disk so boot loader
  picks it up next start.

### F16 · Summary fallback no longer Debug-formatted ✅ — done in M15.47

`render_summary` gains explicit ES + EN arms for
`LeadTransitioned`, `MeetingIntent`, `DraftPending` — every
kind now renders human text (`🔄 Lead transicionó · …`,
`📅 Intent de reunión de …`, `✉️ Draft pendiente de revisión …`).
The dedicated renderers (`render_transition_summary`,
`render_intent_summary`) still own those kinds in the active
publish paths; this fallback only fires when a future caller
routes through `classify` for them. 4 new tests (one per kind
+ EN-locale defensive sweep) assert no `Debug` leaks.

## 🔵 Low · nice-to-have

### F27 · `/marketing/audit` UI tab ✅ — done in agent-creator 0.0.66

`SettingsAudit` component shipped as a new tab in
`/m/marketing/settings/audit`. Reads
`/api/marketing/audit?kind=&lead_id=&limit=` via the
existing `getAudit()` API client.

UI layout:
- Filter strip (Kind dropdown · Lead id input · Límite
  numeric · Aplicar / Limpiar buttons). Draft state until
  Aplicar fires, so typing a lead id doesn't fire a GET
  per keystroke.
- Empty / loading / error states (each with concrete copy
  surfacing tenant scoping).
- Row format: timestamp + kind badge (color-keyed by
  variant) + optional lead_id mono + summary + expand
  caret. Click expands an indented JSON pre block for the
  full row.

`summarizeAuditEvent(e)` distills each variant into a
one-line summary:
- `routing_decided` → `→ <seller> · <email> (rule:<id>) · score N`
  (or `dropped · …` on null seller)
- `lead_transitioned` → `<from> → <to> · <reason>`
- `notification_published` → `<kind> · <channel> · seller <id>`
- `topic_guardrail_fired` → `<rule_name> (<action>) · "<excerpt>"`
- `duplicate_person_detected` → `<conf%> · <signal> · <detail>`

Exhaustive `switch` on the discriminator forces a TS
compile error if a future audit kind lands without a
renderer.

10 vitest unit cases cover every variant + edge cases:
seller-arrow vs dropped, rule-id suffix when present,
empty reason on lead_transitioned, deduped channel on
notification_published, percent rounding on
duplicate_person_detected.

108/108 frontend tests green (98 baseline + 10 F27). Build
clean (579 kB main bundle).

Bump:
- agent-creator-frontend 0.0.78 → 0.0.79
- agent-creator 0.0.65 → 0.0.66

### F28 · Smoke test E2E vs real mailbox (M15.26)

- **Origin:** M15.26 done-criterion never executed.
- **Plan:** operator runs the extension against a real
  Gmail / Outlook mailbox with a small test seller list.
  Validates DKIM/SPF, threading, deliverability, anti-loop.
  Captures any framework gap as a fresh sub-phase in
  `proyecto/PHASES-microapps.md`.
- **Effort:** wall-clock 30-60 min smoke session, no LOC.
- **Blocker:** M22 outbound draft pipeline (smoke needs
  end-to-end send to validate).

### F29 · SDK lift sweep — final pass ✅ — done in SDK 0.1.11 / marketing 0.17.2

Reviewed all 9 marketing-touched modules from M15.23.x +
M17.3 + F25 against the SDK lift heuristics.

**Lifted (2):**

- `crate::threading` → `nexo_microapp_sdk::email_threading`
  (feature `email-threading`). Pure string helpers
  (`normalize_subject` + `synth_thread_id`) — useful for
  any inbound-email consumer. 22 unit tests moved with
  the module. Marketing keeps a 4-line re-export shim.
- `crate::notification_dedup` → `nexo_microapp_sdk::dedup`
  (feature `dedup`, optional `dedup-sled`). Generic
  time-bucketed dedup primitive over
  `(tenant, X, Y, minute)` keys + the F25 sled backend.
  14 unit tests moved. Marketing's `dedup-sled` feature
  forwards to SDK's identically-named feature.

**Stayed marketing-specific (7):**

- `crate::tracking` — 60-LOC glue using `crate::tenant::TenantId`.
- `crate::audit` — `AuditEvent` carries CRM-shaped variants.
- `crate::scoring` — `LeadCtx` + 5 marketing-default rules.
- `crate::guardrails` — 1-line ArcSwap alias.
- `crate::duplicate` — orchestrator is CRM; pure helpers
  (30 LOC) defer until 2nd consumer.
- `crate::availability` — takes CRM-shaped `Seller` directly.
- `crate::whatsapp_ingest` — subscriber pipeline is the
  right layer for SDK consumption.

Each "stay" module gained a "**F29 sweep:** marketing-
specific by design" comment block so the next reviewer
doesn't re-litigate.

Tests: SDK 332 → 360 (+28 lifted). Marketing 365 → 334
(modules moved; net 0 effective tests lost — every
behavioural assertion runs in the SDK now).

Versions: SDK 0.1.10 → 0.1.11 · marketing 0.17.1 → 0.17.2.

### F30 · Frontend granular guardrail compile errors ✅ — done in marketing 0.16.2 / agent-creator 0.0.65

Backend + frontend halves.

**Backend (marketing 0.16.2):**
`PUT /config/topic_guardrails` now returns a structured
`detail` object on `guardrail_compile` failure with:
- `kind`: `"invalid_pattern" | "duplicate_id" | "empty_rule"`
- `rule_id`: operator-authored id (or null when the loader
  surfaces no rule context)
- `pattern_index`: 0-based index for `invalid_pattern`,
  null otherwise
- `regex_error`: underlying `regex` crate message for
  `invalid_pattern`, null otherwise

The `Display` `message` field stays for legacy clients;
new consumers read `detail.*` for typed UX.

**Frontend (agent-creator 0.0.65 / frontend 0.0.78):**
`parseGuardrailCompileDetail(err)` extracts the structured
detail from `HttpError.body`, resilient to missing /
non-string fields (returns `null` cleanly). Modal `onSave`
captures the detail, optimistically swaps the displayed
rule list with the operator's staged edit so the offending
rule (which isn't persisted yet) IS visible in the
highlighted list while the operator iterates.

UI changes in the rule list:
- Offending rule row gets a red border + ring + amber
  badge with the kind label (`invalid_pattern` /
  `duplicate_id` / `empty_rule`).
- For `invalid_pattern`, the specific pattern at
  `pattern_index` gets a red highlight + tooltip showing
  the regex error.
- Banner above the list summarises via
  `summarizeGuardrailCompileDetail` ("Regla \`pricing\`
  patrón #2 inválido: regex parse error: …").
- Highlight persists until the next successful save (which
  clears `compile_error` state).

12 new vitest unit cases covering both helpers: parse
returns null for non-HttpError / wrong code / missing
detail / unknown kind, parses all three variants, survives
partial detail with non-string fields; summarize renders
each kind with proper labels + falls back when fields
missing.

98/98 frontend tests green (86 baseline + 12 F30). 360/360
marketing tests green. Versions: marketing 0.16.1 → 0.16.2 ·
agent-creator 0.0.64 → 0.0.65 · frontend 0.0.77 → 0.0.78.

### F11 · `working_hours` + `alt_emails` editable ✅ — done in M15.45

`SellerForm` gains:
- `ChipInput` component for `alt_emails` with Enter/comma to
  add, backspace on empty to remove last, click × to remove.
- Working hours editor (toggle to enable, IANA timezone text
  input, 3 weekday rows: mon_fri / saturday / sunday — each
  with enabled checkbox + HH:MM start/end time inputs).
- `buildPayload` validates HH:MM format, start < end per
  enabled window, valid alt_emails.
- `pickFormState` round-trips both blocks on edit.

### F12 · Agent UI badge filters on click ✅ — done in M15.46

`Agents.tsx` badge click navigates to
`/m/marketing/settings/sellers?agent_id=<id>`. The sellers
tab reads the param via `useSearchParams`, filters
`slice.data` at render-time + renders a violet "🔎 Filtrado a
sellers de pedro-agent (3 de 12)" banner with a "Quitar
filtro" button that strips the URL param.

### F13 · Edit-from-agent path ✅ — done in M15.54

`Agents.tsx` edit modal's old "Bindings (solo lectura)"
section split into:
- "📧 Email sellers" — editable. Each marketing binding
  renders a row with the seller id (deeplink to
  `/m/marketing/settings/sellers?agent_id=<id>`) + a
  "× Desvincular" button. A "+ Vincular seller" picker at
  the bottom lists every seller not yet bound to this agent
  (sellers bound elsewhere show "(mover desde otro-agent)" —
  picking moves the binding).
- "Otros bindings (solo lectura)" — whatsapp / telegram /
  future, unchanged from before.

Two new helpers in `api/agents.ts`:
- `bindSellerToAgent(seller_id, target_agent_id)` — patches
  the matching seller's `agent_id` + saves + reconciles.
- `unbindSellerFromAgent(seller_id)` — strips `agent_id` +
  `notification_settings` + `model_override` + saves +
  reconciles.

Both run the M15.37 reconciler with `previousSellers` (so
the F6 fast path applies — only the affected agents are
walked) and return the reconcile outcome so the modal
surfaces partial-failure banners.

Modal state: `bindings_busy` + `bindings_error` + `bind_pick`.
Post-bind/unbind: `refresh_sellers` (re-fetch the full list
+ regroup by `agent_id` for the badge column) + a targeted
`agents/get` to refresh `draft.inbound_bindings` without
losing unsaved system_prompt / model edits.

6 new vitest cases in `tests/api/agent-seller-bind.test.ts`
covering bind / move / unbind / strip-aux-fields / no-match
no-op / error bubble.

### F14 · `/agents` badge stays fresh ✅ — done in M15.51

`Agents.tsx` polls `getSellers` every 30 s while the tab is
visible (skips the poll on `document.visibilityState !==
"visible"` to avoid waking idle laptops). `visibilitychange`
listener triggers a bonus refresh on tab focus so operators
tabbing back from `/m/marketing/settings/sellers` see the
badge update immediately instead of waiting up to 30 s.

### F17 · Marketing binding distinctly surfaced ✅ — done in M15.49

`Agents.tsx` edit modal's "Bindings" section now branches on
`plugin === "marketing"` and renders a violet `📧 Email · vía
seller pedro` row instead of the generic mono `marketing ·
pedro`. Click the seller id → deeplinks to
`/m/marketing/settings/sellers?agent_id=<id>` (reuses M15.46
filter). Other plugins keep the original mono row.

## ⚪ Resolved during M15.39

### F1 · Forwarder bridge ✅

- **Resolved:** M15.39 — `crate::forwarder` consumes
  `agent.email.notification.*` from the same plugin process and
  routes to `plugin.outbound.{whatsapp,email}.<instance>`. Targets
  baked at frontend save-time, no admin RPC needed from subprocess.

### F3 · `Email{to}` lacked `from` ✅

- **Resolved:** M15.39 — `NotificationChannel::Email` now carries
  `from_instance` (resolved at save) + `to`.

### F5 · Partial-warning UX ✅

- **Resolved:** M15.37 already keeps the modal open with a banner
  on `ok_with_partial_warning`. Acceptable UX — operator dismisses
  manually.

### F18 · Reconciler doesn't handle seller delete / rename ✅

- **Resolved:** delete works (reconciler runs with the new list
  sans removed). Rename is documented as not-supported (id
  immutable in form via `disabled` attr).

# Follow-ups · marketing extension

Open follow-ups across the M15 milestones. Triaged by severity.
Each entry: origin milestone · estimated effort · acceptance criteria.

## 🟠 High · UX visible

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

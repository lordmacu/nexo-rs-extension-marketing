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

### F4 · Reconciliation race condition (M15.37)

- **Origin:** M15.37 (microapp)
- **Status:** `saveSellers` + `agents/upsert` are separate
  client-side calls. 2 operators saving simultaneously can produce
  inconsistent agent bindings.
- **Impact:** rare in 1-operator deployments; real at scale.
- **Plan:** move reconciliation to a microapp backend route
  behind a per-tenant mutex. OR: document "single-operator
  recommended" in the README.
- **Recommendation:** doc-only for now.

### F7 · Stale `seller.agent_id` when agent deleted ✅ — done in M15.42

Frontend `unbindSellersFromAgent` runs before
`agents/delete`; cascade banner surfaces affected sellers
in the confirm modal. Dropdown filters inactive agents.

### F8 · `LeadReplied` notification ✅ — done in M15.40

## 🟡 Medium · technical debt

### F6 · Reconciler walk O(N agents) per save

- **Origin:** M15.37
- **Status:** every seller save → `agents/list` + per-agent
  `agents/get` + diff. ~200ms with 5 agents; 2-3s with 50.
- **Plan:** compute the diff client-side comparing previous vs
  new sellers list — only touch agents that appear in old
  OR new `agent_id` set.
- **Effort:** ~60 LOC + reconciler signature change.

### F9 · Notification deduplication missing

- **Origin:** M15.38
- **Status:** marketing extension restart mid-broker-hop →
  broker re-delivers → notification publishes twice.
- **Impact:** ocasional duplicate ping.
- **Plan:** dedup key `(tenant, lead_id, kind, at_ms_bucket=60s)`
  in a `sled` cache 1h; lookup before publish.
- **Effort:** ~80 LOC, 1 new dep.

### F10 · Operator-supplied summary templates ✅ — done in M15.44

`notification_templates.yaml` per tenant, loaded via
`load_notification_templates`, threaded through every
classifier as `Option<&TemplateLookup>`. Render functions try
the operator template first via `nexo-tool-meta::template`'s
`{{path}}` syntax, fall back to hardcoded ES/EN. Live-reload
via `arc_swap` on `PUT /config/notification_templates`.
Frontend Settings tab "Templates" lets operators edit via
JSON editor (M15.34 pattern).

### F15 · No integration test for seller lookup live-reload

- **Origin:** M15.38
- **Status:** unit tests separate classifier (pure) from publish
  (IO). No end-to-end test that PUT sellers via admin router
  → next broker hop sees fresh settings.
- **Plan:** integration test `tests/notification_live_reload.rs`
  that boots admin router + broker hop test fixtures.
- **Effort:** ~150 LOC.

### F16 · Summary fallback for non-LeadCreated kinds is `Debug`

- **Origin:** M15.38
- **Status:** `render_summary` only branches on `LeadCreated`;
  other kinds fall through to `format!("{:?}", kind)` — operator
  sees `📧 LeadTransitioned · ...` instead of human text.
- **Plan:** complete templates for the 3 remaining kinds when
  F2 lands.
- **Effort:** ~30 LOC (paired with F2).

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

### F12 · Agent UI badge doesn't filter on click

- **Origin:** M15.36
- **Status:** `📧 N email sellers` navigates to the full
  sellers list — no filter applied.
- **Plan:** querystring `?agent_id=pedro-agent` + sidebar
  applies via `useUrlState`.
- **Effort:** ~30 LOC.

### F13 · No edit-from-agent path

- **Origin:** M15.36
- **Status:** to associate / disassociate seller from agent,
  operator must navigate to the marketing tab.
- **Plan:** "Email sellers" section in agent edit modal with
  add/remove inline.
- **Effort:** ~150 LOC.

### F14 · Stale data in `/agents` count badge

- **Origin:** M15.36
- **Status:** badge fetches sellers once on mount; no refresh
  on cross-tab edit.
- **Plan:** subscribe to firehose (extend with `seller.changed`
  topic) or 30s polling.
- **Effort:** depends on approach.

### F17 · Agent UI doesn't surface `marketing` binding distinctly

- **Origin:** M15.37
- **Status:** the auto-bound `{plugin: "marketing", instance: pedro}`
  renders as a generic binding row in the agent UI — no link
  back to the seller.
- **Plan:** detect `plugin === "marketing"` + render "Email
  channel via seller X" with deeplink.
- **Effort:** ~50 LOC.

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

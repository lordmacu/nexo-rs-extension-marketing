# Follow-ups · marketing extension

Open follow-ups across the M15 milestones. Triaged by severity.
Each entry: origin milestone · estimated effort · acceptance criteria.

## 🟠 High · UX visible

### F2 · Publish notifications for the 3 remaining kinds

- **Origin:** M15.38
- **Status:** wire shape defines `LeadCreated`, `LeadTransitioned`,
  `DraftPending`, `MeetingIntent` — only `LeadCreated` publishes today.
- **Impact:** operator enables "Transición de estado" toggle + never
  receives anything → broken expectation.
- **Plan:**
  - `LeadTransitioned` — fire from `LeadStore::transition()` after
    the new state lands. Wire `maybe_notify_transitioned`
    classifier in `notification.rs`.
  - `MeetingIntent` — fire from `tools::lead_detect_meeting_intent`
    when `confidence >= 0.7`.
  - `DraftPending` — defer to M22 (draft pipeline doesn't ship
    until then).
- **Effort:** ~120 LOC + 3 new classifier tests.

### F4 · Reconciliation race condition (M15.37)

- **Origin:** M15.37 (microapp)
- **Status:** `saveVendedores` + `agents/upsert` are separate
  client-side calls. 2 operators saving simultaneously can produce
  inconsistent agent bindings.
- **Impact:** rare in 1-operator deployments; real at scale.
- **Plan:** move reconciliation to a microapp backend route
  behind a per-tenant mutex. OR: document "single-operator
  recommended" in the README.
- **Recommendation:** doc-only for now.

### F7 · Stale `vendedor.agent_id` when agent deleted

- **Origin:** M15.35
- **Status:** if the operator deletes an agent via `/agents`,
  vendedores referencing that agent silently fail (publish to
  topic with no consumers).
- **Plan:**
  - Microapp's `agents/delete` handler scans vendedores +
    null-eares the `agent_id`.
  - VendedorForm's agent dropdown filters `active=true`.
- **Effort:** ~80 LOC.

### F8 · `notify_on_thread_bumped` missing

- **Origin:** M15.38
- **Status:** broker hop fires `LeadFirehoseEvent::ThreadBumped`
  for inbound on existing threads — no notification kind
  corresponding.
- **Impact:** operator misses "client replied" pings —
  conversation-in-progress UX gap.
- **Plan:** add `EmailNotificationKind::LeadReplied` (or rename
  ThreadBumped). Toggle `on_lead_replied` in settings. Publish
  from the existing thread-bump branch in `broker.rs`.
- **Effort:** ~80 LOC + 1 form checkbox.

## 🟡 Medium · technical debt

### F6 · Reconciler walk O(N agents) per save

- **Origin:** M15.37
- **Status:** every vendedor save → `agents/list` + per-agent
  `agents/get` + diff. ~200ms with 5 agents; 2-3s with 50.
- **Plan:** compute the diff client-side comparing previous vs
  new vendedores list — only touch agents that appear in old
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

### F10 · Hardcoded ES/EN summary template

- **Origin:** M15.38
- **Status:** `render_summary` has hardcoded strings; no
  operator-supplied template per kind / locale.
- **Plan:** template in `marketing.yaml` with placeholders
  `{from}`, `{subject}`, `{vendedor}`. Render via
  `nexo-tool-meta::template`.
- **Effort:** ~100 LOC.

### F15 · No integration test for vendedor lookup live-reload

- **Origin:** M15.38
- **Status:** unit tests separate classifier (pure) from publish
  (IO). No end-to-end test that PUT vendedores via admin router
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

### F11 · `working_hours` + `alt_emails` not editable in form

- **Origin:** M15.35
- **Status:** form preserves whatever is in YAML; no UI to
  create/change.
- **Plan:** mini-editor for each. Working hours = 7 weekday
  slider rows + timezone picker. alt_emails = chip input.
- **Effort:** ~200 LOC.

### F12 · Agent UI badge doesn't filter on click

- **Origin:** M15.36
- **Status:** `📧 N email vendedores` navigates to the full
  vendedores list — no filter applied.
- **Plan:** querystring `?agent_id=pedro-agent` + sidebar
  applies via `useUrlState`.
- **Effort:** ~30 LOC.

### F13 · No edit-from-agent path

- **Origin:** M15.36
- **Status:** to associate / disassociate vendedor from agent,
  operator must navigate to the marketing tab.
- **Plan:** "Email vendedores" section in agent edit modal with
  add/remove inline.
- **Effort:** ~150 LOC.

### F14 · Stale data in `/agents` count badge

- **Origin:** M15.36
- **Status:** badge fetches vendedores once on mount; no refresh
  on cross-tab edit.
- **Plan:** subscribe to firehose (extend with `vendedor.changed`
  topic) or 30s polling.
- **Effort:** depends on approach.

### F17 · Agent UI doesn't surface `marketing` binding distinctly

- **Origin:** M15.37
- **Status:** the auto-bound `{plugin: "marketing", instance: pedro}`
  renders as a generic binding row in the agent UI — no link
  back to the vendedor.
- **Plan:** detect `plugin === "marketing"` + render "Email
  channel via vendedor X" with deeplink.
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

### F18 · Reconciler doesn't handle vendedor delete / rename ✅

- **Resolved:** delete works (reconciler runs with the new list
  sans removed). Rename is documented as not-supported (id
  immutable in form via `disabled` attr).

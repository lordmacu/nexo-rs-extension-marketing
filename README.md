# `nexo-rs-extension-marketing`

Email marketing extension for the [nexo-rs] agent framework —
multi-tenant CRM-lite that watches IMAP mailboxes, resolves
sender identity (multi-email per person, web-scraper enrichment
for corporate domains), routes inbound to vendedores via YAML
rules, generates AI draft replies the operator approves in the
[`agent-creator` microapp][agent-creator] UI, schedules
followups, and tracks lead state through `cold → engaged →
meeting_scheduled → qualified | lost`.

Sibling of [`nexo-rs-plugin-browser`][browser]. Subprocess
plugin per the [Phase 81.5 plugin contract][contract].

## Status

✅ **End-to-end pipeline shipped (v0.1.0).**

M15 milestones A–I landed: lead state machine + per-tenant
sqlite store, identity resolver with 5 fallback adapters,
corporate domain scraper, broker decoder + outbound
publisher, 6 tool handlers, compliance gate (anti-loop +
opt-out + PII redactor + rate-limit), HTTP admin axum
router, agent-creator microapp proxy, and a release-blocker
cross-tenant isolation suite (8 assertions green).

**152/152 tests** green (138 unit + 8 integration + 6
microapp proxy).

Pending follow-ups:
- CRUD admin endpoints for rules / mailboxes / vendedores /
  followup_profiles (need YAML write helpers).
- SSE firehose `/firehose` → tenant-scoped
  `agent.lead.transition.*`.
- E2E smoke against a fake IMAP server (`testcontainers`).
- `cargo install` from crates.io once the framework SDK lifts
  publish (currently consumes nexo-microapp-sdk via path).

## Installation (planned)

```bash
cargo install nexo-rs-extension-marketing

# Copy the binary + manifest under the operator's plugin
# discovery path (defaults to `${state}/plugins/`):
cp $(which nexo-marketing) ${state}/plugins/marketing/bin/
cp $(cargo locate-project --workspace --message-format plain | xargs dirname)/nexo-plugin.toml ${state}/plugins/marketing/
```

The daemon's plugin discovery walker (Phase 81.5) auto-spawns
the subprocess on next boot.

## Configuration (planned)

Per-tenant YAML lives under `${NEXO_EXTENSION_STATE_ROOT}/marketing/<tenant_id>/`:
- `mailboxes.yaml` — IMAP accounts to watch
- `vendedores.yaml` — sales reps + outbound SMTP creds + working hours
- `rules.yaml` — routing rules
- `followup_profiles.yaml` — cadence templates
- `leads.db` — sqlite store with lead state + thread history

See `config/*.yaml.example` (added in subsequent commits).

## Capabilities

The plugin manifest (`nexo-plugin.toml`) declares:

- `nexo_capabilities`: `broker`, `memory`
- `[plugin.subscriptions].broker_topics`: `plugin.inbound.email.*`
- `[plugin.extends].tools` (6): `marketing_lead_profile`,
  `marketing_lead_route`, `marketing_lead_schedule_followup`,
  `marketing_lead_mark_qualified`,
  `marketing_lead_detect_meeting_intent`,
  `marketing_lead_followup_sweep`
- `[plugin.sandbox]`: bubblewrap with `network = "host"` (the
  scraper needs egress)
- `[plugin.http_server]`: loopback admin API on port
  `${MARKETING_HTTP_PORT}` (default `18766`), bearer auth via
  `${MARKETING_ADMIN_TOKEN}`

## License

Dual-licensed under MIT or Apache-2.0, at your option.

[nexo-rs]: https://github.com/lordmacu/nexo-rs
[browser]: https://github.com/lordmacu/nexo-rs-plugin-browser
[agent-creator]: https://github.com/lordmacu/agent-creator-microapp
[contract]: https://github.com/lordmacu/nexo-rs/blob/main/nexo-plugin-contract.md

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

✅ **End-to-end pipeline shipped (v0.5.0).**

The extension is now a real Phase 81.5 plugin: the daemon
spawns `nexo-marketing` as a subprocess, hands it `tool.invoke`
calls over JSON-RPC stdio, and routes `plugin.inbound.email.*`
broker events through to the inbound decoder. The HTTP loopback
admin (consumed by the agent-creator microapp) keeps running
alongside on the same process.

M15 milestones A–I + M15.27 (plugin contract):
- Lead state machine + per-tenant sqlite store
- Identity resolver with 5 fallback adapters
- Corporate domain scraper + 75-personal / 30-disposable list
- Inbound decoder + outbound publisher with idempotency
- 6 tool handlers exposed via `marketing_lead_*` (LLM-callable
  through the daemon's tool catalogue)
- Compliance gate: anti-loop + opt-out + PII redactor +
  per-recipient rate-limit
- HTTP admin axum router → microapp proxy
- Cross-tenant isolation suite (8 assertions, release-blocker)
- **Stdio JSON-RPC plugin contract** + broker subscriber

**184/184 tests** green (138 unit + 8 cross-tenant + 6
microapp proxy + 25 plugin / firehose / admin + 7 thread).
Lead lifecycle events stream live via `/firehose` SSE; lead
threads persist to sqlite and serve through
`GET /leads/:lead_id/thread`.

The broker hop now drives the full pipeline: each inbound runs
the SDK `FallbackChain` (with `display_name` + `reply_to`
adapters today) → upserts via the SDK identity stores → routes
through the YAML rule set → creates the lead with real
`why_routed` audit. LLM extractor + scraper adapters arrive in
M22 once the operator's LLM client + scraper config land.

Pending follow-ups:
- LLM extractor + scraper adapters wired into the chain
  (M22) — pure additive, no changes to the broker hop's
  contract.
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

//! Broker subscriber for `plugin.inbound.email.*`.
//!
//! Registered via `PluginAdapter::on_broker_event`. The
//! daemon routes every event whose topic matches one of the
//! manifest's `[plugin.subscriptions].broker_topics` patterns
//! to this handler. We:
//!
//! 1. Filter by topic prefix `plugin.inbound.email.` so other
//!    subscriptions added later don't leak through (defense
//!    in depth — the manifest already filters server-side).
//! 2. Pull the wire fields we need (`account_id`, `instance`,
//!    `uid`, `raw_bytes`) without depending on the email
//!    plugin crate; we use our own private deserialise struct
//!    so the marketing extension stays decoupled.
//! 3. Run `decode_inbound_email` (RFC 5322) → `ParsedInbound`.
//! 4. Run the identity resolver chain → upsert Person via the
//!    SDK identity stores. Disposable senders short-circuit
//!    here. Pending outcomes still create a lead with a
//!    deterministic `placeholder-<uuid5>` person id so the
//!    operator can confirm in the UI.
//! 5. Run the YAML routing dispatcher → pick a seller.
//!    `Drop` rule outcome aborts; `NoTarget` (empty
//!    round-robin pool) falls back to `unassigned`.
//! 6. Look up or create a lead in the per-tenant store.
//!
//! Full draft generation + outbound dispatch lives downstream
//! (M22) — this commit lands the resolver + router so the
//! pipeline is real end-to-end up to lead creation.

use chrono::Utc;
use nexo_microapp_sdk::enrichment::{EnrichmentInput, FallbackChain, FallbackOutcome};
use nexo_microapp_sdk::identity::{Person, PersonEmailStore, PersonStore};
use nexo_microapp_sdk::plugin::BrokerSender;
use nexo_tool_meta::marketing::{
    EnrichmentResult, EnrichmentStatus, LeadId, PersonId, PersonInferred, TenantIdRef,
    SellerId,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::broker::inbound::{decode_inbound_email, ParseError, ParsedInbound};
use crate::firehose::{LeadEventBus, LeadFirehoseEvent};
use crate::lead::{
    LeadRouter, LeadStore, MessageDirection, NewLead, NewThreadMessage, RouteInputs,
    RouteOutcome,
};
use crate::notification::{
    maybe_notify_lead_created, maybe_notify_lead_replied, NotificationOutcome,
    SellerLookup,
};
use crate::plugin::IdentityDeps;
use crate::tenant::TenantId;

/// Wire-shape mirror of `nexo_plugin_email::events::InboundEvent`,
/// kept private so the extension doesn't depend on the email
/// plugin crate. Only the fields the decoder needs are pulled
/// — meta + attachments are ignored at this stage (decoder
/// re-parses raw_bytes anyway).
#[derive(Debug, Deserialize)]
struct InboundWire {
    account_id: String,
    instance: String,
    uid: u32,
    #[serde(with = "serde_bytes")]
    raw_bytes: Vec<u8>,
}

/// Parameter-light enum so the handler returns a structured
/// outcome the unit tests can assert on.
#[derive(Debug, PartialEq, Eq)]
pub enum HandledOutcome {
    /// Topic didn't match `plugin.inbound.email.*` — ignored.
    Skipped,
    /// Wire decode failed (malformed payload). Logged + dropped.
    Malformed,
    /// RFC 5322 parse failure (corrupt bytes / disposable sender).
    /// Logged + dropped — disposables go through this path on
    /// purpose so the audit trail captures them.
    DecodeFailed(String),
    /// Routing rule dropped the inbound (operator-configured).
    DroppedByRule { rule_id: Option<String> },
    /// Existing thread already tracked — bumped activity stamp.
    LeadUpdated { lead_id: LeadId, thread_id: String },
    /// Cold thread → new lead created. `resolver_source` is
    /// the chain adapter that produced the hit (`"placeholder"`
    /// when the chain returned Pending and we used the
    /// deterministic uuid5 fallback).
    LeadCreated {
        lead_id: LeadId,
        thread_id: String,
        resolver_source: String,
    },
}

/// Deterministic person id from a lowercased email — same
/// algorithm as `tools::lead_profile::derive_placeholder_person_id`.
/// Stable across calls so the same sender keeps the same id
/// when the resolver lands a Pending verdict.
fn placeholder_person_id(email: &str) -> PersonId {
    let ns = Uuid::NAMESPACE_DNS;
    let v5 = Uuid::new_v5(&ns, email.to_ascii_lowercase().as_bytes());
    PersonId(format!("placeholder-{v5}"))
}

/// Build a `Person` record from a resolver hit. `inferred` may
/// carry the name extracted by an adapter; we fall back to the
/// email's local part when not.
fn person_from_resolver_hit(
    tenant_id: &TenantId,
    email: &str,
    display_name: Option<&str>,
    result: &EnrichmentResult,
) -> Person {
    let now_ms = Utc::now().timestamp_millis();
    let person_inferred = result.person_inferred.clone();
    // Prefer (resolver inferred name) > (display name from From) > (email local part).
    let primary_name = person_inferred
        .as_ref()
        .and_then(|p: &PersonInferred| p.name.clone())
        .or_else(|| display_name.map(|s| s.to_string()))
        .unwrap_or_else(|| {
            email
                .split_once('@')
                .map(|(local, _)| local.to_string())
                .unwrap_or_else(|| email.to_string())
        });
    Person {
        id: placeholder_person_id(email),
        tenant_id: TenantIdRef(tenant_id.as_str().into()),
        primary_name,
        primary_email: email.to_ascii_lowercase(),
        alt_emails: Vec::new(),
        company_id: None,
        // Map the resolver source label onto the typed enum.
        // `signature` / `display_name` / `llm_extractor` / cross-
        // thread are the supported branches; anything else lands
        // as `None` (the operator confirms manually).
        enrichment_status: match result.source.as_str() {
            "signature" | "display_name" => EnrichmentStatus::SignatureParsed,
            "llm_extractor" => EnrichmentStatus::LlmExtracted,
            "cross_thread" => EnrichmentStatus::CrossLinked,
            "reply_to" => EnrichmentStatus::SignatureParsed,
            _ => EnrichmentStatus::None,
        },
        enrichment_confidence: result.confidence,
        tags: Vec::new(),
        created_at_ms: now_ms,
        last_seen_at_ms: now_ms,
    }
}

/// Build a Person record for the Pending resolver path — no
/// adapter cleared the threshold but we still want a stable
/// identity row so cross-thread linking works on subsequent
/// inbounds from the same address.
fn person_from_pending(
    tenant_id: &TenantId,
    email: &str,
    display_name: Option<&str>,
) -> Person {
    let now_ms = Utc::now().timestamp_millis();
    Person {
        id: placeholder_person_id(email),
        tenant_id: TenantIdRef(tenant_id.as_str().into()),
        primary_name: display_name
            .map(|s| s.to_string())
            .or_else(|| {
                email
                    .split_once('@')
                    .map(|(local, _)| local.to_string())
            })
            .unwrap_or_else(|| email.to_string()),
        primary_email: email.to_ascii_lowercase(),
        alt_emails: Vec::new(),
        company_id: None,
        enrichment_status: EnrichmentStatus::None,
        enrichment_confidence: 0.0,
        tags: Vec::new(),
        created_at_ms: now_ms,
        last_seen_at_ms: now_ms,
    }
}

/// Run the resolver chain against the parsed email. Returns
/// `(person, source_label)`. Disposable detection happens
/// upstream in `decode_inbound_email`, so we don't re-check
/// here — every parsed email is already a non-disposable.
async fn resolve_person(
    tenant_id: &TenantId,
    parsed: &ParsedInbound,
    chain: &FallbackChain,
) -> (Person, String) {
    let input = EnrichmentInput {
        from_email: &parsed.from_email,
        from_display_name: parsed.from_display_name.as_deref(),
        subject: &parsed.subject,
        body_excerpt: &parsed.body_excerpt,
        reply_to: parsed.reply_to.as_deref(),
    };
    match chain.run(&input).await {
        FallbackOutcome::Hit { result, .. } => {
            let label = result.source.clone();
            (
                person_from_resolver_hit(
                    tenant_id,
                    &parsed.from_email,
                    parsed.from_display_name.as_deref(),
                    &result,
                ),
                label,
            )
        }
        FallbackOutcome::AllExhausted { .. } => (
            person_from_pending(
                tenant_id,
                &parsed.from_email,
                parsed.from_display_name.as_deref(),
            ),
            "placeholder".into(),
        ),
    }
}

/// Run the routing dispatcher against the parsed email + the
/// resolved person's hint metadata. Returns the chosen
/// `SellerId` + the audit `why_routed` chain.
fn route_inbound(
    parsed: &ParsedInbound,
    router: &LeadRouter,
    sellers: Option<&SellerLookup>,
    now_utc: chrono::DateTime<chrono::Utc>,
) -> Result<(Option<SellerId>, Vec<String>), crate::error::MarketingError> {
    let inputs = RouteInputs {
        sender_email: Some(parsed.from_email.clone()),
        sender_domain_kind: Some(parsed.from_domain_kind),
        company_industry: None,
        person_tags: Vec::new(),
        score: None,
        body: Some(parsed.body_excerpt.clone()),
        subject: Some(parsed.subject.clone()),
    };
    let outcome = router.route(&inputs)?;
    Ok(match outcome {
        RouteOutcome::Seller {
            seller_id,
            matched_rule_id,
            why,
        } => {
            let mut audit = why;
            if let Some(r) = matched_rule_id {
                audit.push(format!("rule:{r}"));
            }
            // M15.23.g — availability gate. When the matched
            // seller is on vacation or outside their working
            // hours, fall through to `unassigned` with an
            // audit reason so the operator UI can surface
            // why the lead landed in the no-owner queue.
            if let Some(lookup) = sellers {
                if let Some(seller) = resolve_seller(lookup, &seller_id) {
                    let outcome = crate::availability::check(&seller, now_utc);
                    if !outcome.is_available() {
                        if let Some(reason) = outcome.audit_reason() {
                            audit.push(format!("seller:{}:{}", seller_id.0, reason));
                        }
                        audit.push("availability_fallback_unassigned".into());
                        return Ok((Some(SellerId("unassigned".into())), audit));
                    }
                }
            }
            (Some(seller_id), audit)
        }
        RouteOutcome::Drop {
            matched_rule_id,
            why,
        } => {
            let mut audit = why;
            audit.push("drop_by_rule".into());
            if let Some(r) = matched_rule_id {
                audit.push(format!("rule:{r}"));
            }
            (None, audit)
        }
        RouteOutcome::NoTarget { why, .. } => {
            let mut audit = why;
            audit.push("no_target_fallback_unassigned".into());
            (Some(SellerId("unassigned".into())), audit)
        }
    })
}

/// Snapshot the seller record for the matched id off the
/// arc_swap-backed lookup. Returns `None` when the operator
/// removed the seller after the rule was authored — gate
/// then defaults to "available" because we have no record to
/// evaluate against (matches the existing
/// `lookup-misses-fall-through-to-route` posture).
fn resolve_seller(
    lookup: &SellerLookup,
    seller_id: &SellerId,
) -> Option<nexo_tool_meta::marketing::Seller> {
    let snapshot = lookup.load_full();
    snapshot.get(seller_id).cloned()
}

/// Build a `NewThreadMessage` from a parsed inbound. Uses the
/// RFC 5322 `Message-Id` as the dedupe key; falls back to the
/// thread id + timestamp combo when the header is missing
/// (rare — most servers stamp one).
fn inbound_message_from_parsed(parsed: &ParsedInbound, fallback_at_ms: i64) -> NewThreadMessage {
    NewThreadMessage {
        message_id: parsed
            .message_id
            .clone()
            .unwrap_or_else(|| format!("synth:{}-{}", parsed.thread_id, fallback_at_ms)),
        direction: MessageDirection::Inbound,
        from_label: parsed
            .from_display_name
            .clone()
            .unwrap_or_else(|| parsed.from_email.clone()),
        body: parsed.body_excerpt.clone(),
        at_ms: fallback_at_ms,
        draft_status: None,
    }
}

/// Persist the resolved person + email link. Errors are logged
/// + downgraded to a placeholder id so a transient store
/// failure doesn't lose the inbound (the lead still gets
/// created; the operator can resolve later).
async fn persist_person(
    tenant_id: &TenantId,
    person: Person,
    identity: &IdentityDeps,
) -> PersonId {
    match identity.persons.upsert(tenant_id.as_str(), &person).await {
        Ok(p) => {
            // Best-effort link the email → person. Already-linked
            // is a no-op; cross-tenant impossible by primary key.
            if let Err(e) = identity
                .person_emails
                .add(tenant_id.as_str(), &p.id, &p.primary_email, false)
                .await
            {
                tracing::warn!(
                    target: "plugin.marketing.broker",
                    error = %e,
                    person_id = %p.id.0,
                    email = %p.primary_email,
                    "person_email link failed (non-fatal)"
                );
            }
            p.id
        }
        Err(e) => {
            tracing::error!(
                target: "plugin.marketing.broker",
                error = %e,
                "person upsert failed; using placeholder id"
            );
            placeholder_person_id(&person.primary_email)
        }
    }
}

/// Handle one inbound-email broker event. The full pipeline:
/// decode → resolve → route → upsert person → create / bump
/// lead. Each stage logs at appropriate level + the outcome
/// surfaces structured for unit tests + future firehose
/// publishing.
pub async fn handle_inbound_event(
    topic: &str,
    payload: serde_json::Value,
    tenant_id: &TenantId,
    store: &LeadStore,
    router: Option<&LeadRouter>,
    identity: Option<&IdentityDeps>,
    firehose: Option<&LeadEventBus>,
    sellers: Option<&SellerLookup>,
    templates: Option<&crate::notification::TemplateLookup>,
    broker: Option<BrokerSender>,
    dedup: Option<&crate::notification_dedup::DedupCache>,
    audit: Option<&crate::audit::AuditLog>,
) -> HandledOutcome {
    if !topic.starts_with("plugin.inbound.email.") && topic != "plugin.inbound.email" {
        return HandledOutcome::Skipped;
    }

    let wire: InboundWire = match serde_json::from_value(payload) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                target: "plugin.marketing.broker",
                topic, error = %e,
                "malformed plugin.inbound.email payload"
            );
            return HandledOutcome::Malformed;
        }
    };

    let parsed: ParsedInbound = match decode_inbound_email(
        &wire.instance,
        &wire.account_id,
        wire.uid,
        &wire.raw_bytes,
    ) {
        Ok(p) => p,
        Err(ParseError::DisposableSender(addr)) => {
            tracing::info!(
                target: "plugin.marketing.broker",
                from = %addr,
                "dropped disposable sender pre-pipeline"
            );
            return HandledOutcome::DecodeFailed(format!("disposable: {addr}"));
        }
        Err(e) => {
            tracing::warn!(
                target: "plugin.marketing.broker",
                topic,
                error = %e,
                "decode_inbound_email failed"
            );
            return HandledOutcome::DecodeFailed(e.to_string());
        }
    };

    // Existing thread short-circuits everything below — fastest
    // path. Resolver + router are skipped because the lead is
    // already attributed.
    if let Ok(Some(lead)) = store.find_by_thread(&parsed.thread_id).await {
        tracing::debug!(
            target: "plugin.marketing.broker",
            lead_id = %lead.id.0,
            thread_id = %parsed.thread_id,
            "inbound on existing thread"
        );
        // Persist the inbound message — broker delivery is at-
        // least-once but `append_thread_message` is idempotent
        // on `(tenant, lead, message_id)` so replays are safe.
        let now_ms = Utc::now().timestamp_millis();
        let _ = store
            .append_thread_message(
                &lead.id,
                inbound_message_from_parsed(&parsed, now_ms),
            )
            .await;
        if let Some(bus) = firehose {
            bus.publish(LeadFirehoseEvent::ThreadBumped {
                tenant_id: nexo_tool_meta::marketing::TenantIdRef(tenant_id.as_str().into()),
                lead_id: lead.id.clone(),
                thread_id: parsed.thread_id.clone(),
                at_ms: now_ms,
            });
        }
        // M15.40 — operator notification on existing-thread
        // reply. Same publish path as LeadCreated: classifier
        // gates on seller.notification_settings.on_lead_replied
        // + agent_id presence; the forwarder (M15.39) routes to
        // WA / email outbound. Fire-and-forget — never blocks
        // the broker hop on a transient daemon hiccup.
        if let (Some(lookup), Some(broker_sender)) = (sellers, broker.as_ref()) {
            match maybe_notify_lead_replied(tenant_id, lookup, templates, &lead, &parsed) {
                NotificationOutcome::Publish { topic, payload } => {
                    // M15.53 / F9 — dedupe under TTL window so a
                    // NATS redelivery of the same inbound doesn't
                    // ping the operator twice. Cache hit ⇒ skip
                    // publish silently; cache miss records + falls
                    // through to publish.
                    let dedup_key = crate::notification_dedup::DedupKey::new(
                        tenant_id.as_str(),
                        &lead.id.0,
                        "lead_replied",
                        payload.at_ms,
                    );
                    if let Some(cache) = dedup {
                        if cache.is_duplicate(&dedup_key) {
                            tracing::debug!(
                                target: "plugin.marketing.broker",
                                key = %dedup_key.as_str(),
                                "lead_replied notification deduped"
                            );
                            return HandledOutcome::LeadUpdated {
                                lead_id: lead.id.clone(),
                                thread_id: parsed.thread_id,
                            };
                        }
                    }
                    let json_payload = serde_json::to_value(&payload)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    let event = nexo_microapp_sdk::BrokerEvent::new(
                        topic.clone(),
                        "marketing.notification",
                        json_payload,
                    );
                    if let Err(e) = broker_sender.publish(&topic, event).await {
                        tracing::warn!(
                            target: "plugin.marketing.broker",
                            error = %e,
                            topic = %topic,
                            "lead_replied notification publish failed (non-fatal)"
                        );
                    } else {
                        tracing::debug!(
                            target: "plugin.marketing.broker",
                            topic = %topic,
                            channel = ?payload.channel,
                            "lead_replied notification published"
                        );
                    }
                }
                other => {
                    tracing::trace!(
                        target: "plugin.marketing.broker",
                        outcome = ?other,
                        "lead_replied notification skipped"
                    );
                }
            }
        }
        return HandledOutcome::LeadUpdated {
            lead_id: lead.id.clone(),
            thread_id: parsed.thread_id,
        };
    }

    // ─── Resolver pass ────────────────────────────────────────
    let (person_id, resolver_source) = match identity {
        Some(deps) => {
            let (person, source) = resolve_person(tenant_id, &parsed, &deps.chain).await;
            (persist_person(tenant_id, person, deps).await, source)
        }
        None => (
            placeholder_person_id(&parsed.from_email),
            "no_identity_deps".into(),
        ),
    };

    // ─── Router pass ──────────────────────────────────────────
    let (seller_id, mut why_routed) = match router {
        Some(r) => match route_inbound(&parsed, r, sellers, Utc::now()) {
            Ok((Some(v), why)) => (v, why),
            Ok((None, why)) => {
                tracing::info!(
                    target: "plugin.marketing.broker",
                    from = %parsed.from_email,
                    why = ?why,
                    "rule dropped inbound"
                );
                let rule_id = why
                    .iter()
                    .find_map(|s| s.strip_prefix("rule:").map(str::to_string));
                return HandledOutcome::DroppedByRule { rule_id };
            }
            Err(e) => {
                tracing::error!(
                    target: "plugin.marketing.broker",
                    error = %e,
                    "router error; falling back to unassigned"
                );
                (
                    SellerId("unassigned".into()),
                    vec!["router_error_fallback".into()],
                )
            }
        },
        None => (
            SellerId("unassigned".into()),
            vec!["no_router_deps".into()],
        ),
    };

    // ─── Lead create ──────────────────────────────────────────
    why_routed.push(format!("resolver:{resolver_source}"));
    let lead_id = LeadId(Uuid::new_v4().to_string());
    let now_ms = Utc::now().timestamp_millis();
    // M15.23.f — heuristic score on lead create. Composing
    // through the SDK trait keeps the door open for
    // sentiment / intent / BANT scorers to land alongside
    // (each adds its own contributions, the trace stays
    // unified). Score reasons are merged into `why_routed`
    // so the operator audit log surfaces the full
    // explanation in one place.
    let lead_score = crate::scoring::score_lead(&parsed);
    for reason in lead_score.reasons() {
        why_routed.push(format!(
            "score:{}:{}",
            reason.delta, reason.label
        ));
    }
    let new_lead = NewLead {
        id: lead_id.clone(),
        thread_id: parsed.thread_id.clone(),
        subject: parsed.subject.clone(),
        person_id,
        seller_id,
        last_activity_ms: now_ms,
        score: lead_score.value(),
        why_routed,
    };
    match store.create(new_lead).await {
        Ok(lead) => {
            tracing::info!(
                target: "plugin.marketing.broker",
                lead_id = %lead.id.0,
                thread_id = %parsed.thread_id,
                from = %parsed.from_email,
                resolver = %resolver_source,
                "created cold lead from inbound email"
            );
            // M15.23.c — audit row for the routing decision.
            // Fire-and-forget: a failure here logs warn but
            // never sinks the live path — the lead row is
            // already committed.
            if let Some(log) = audit {
                let rule_id = lead
                    .why_routed
                    .iter()
                    .find_map(|s| s.strip_prefix("rule:").map(str::to_string));
                let event = crate::audit::AuditEvent::RoutingDecided {
                    tenant_id: tenant_id.as_str().to_string(),
                    lead_id: Some(lead.id.0.clone()),
                    from_email: parsed.from_email.clone(),
                    chosen_seller_id: Some(lead.seller_id.0.clone()),
                    rule_id,
                    why: lead.why_routed.clone(),
                    score: lead_score.value(),
                    score_reasons: lead_score.reasons().to_vec(),
                    at_ms: now_ms.max(0) as u64,
                };
                if let Err(e) = log.record(event).await {
                    tracing::warn!(
                        target: "plugin.marketing.broker",
                        error = %e,
                        lead_id = %lead.id.0,
                        "audit record failed (non-fatal)"
                    );
                }
            }
            // Seed the thread with the inbound that triggered
            // creation — the operator opens the lead detail and
            // sees the original email immediately.
            let _ = store
                .append_thread_message(
                    &lead.id,
                    inbound_message_from_parsed(&parsed, now_ms),
                )
                .await;
            if let Some(bus) = firehose {
                bus.publish(LeadFirehoseEvent::Created {
                    tenant_id: nexo_tool_meta::marketing::TenantIdRef(
                        tenant_id.as_str().into(),
                    ),
                    lead_id: lead.id.clone(),
                    thread_id: parsed.thread_id.clone(),
                    subject: parsed.subject.clone(),
                    from_email: parsed.from_email.clone(),
                    seller_id: lead.seller_id.0.clone(),
                    state: lead.state,
                    at_ms: now_ms,
                    why_routed: lead.why_routed.clone(),
                });
            }
            // M15.38 — operator notification routed to the
            // bound agent's WA / email channel. Pure decision
            // happens inline; the broker.publish is fire-and-
            // forget so a transient daemon hiccup doesn't
            // sink the lead-create itself.
            if let (Some(lookup), Some(broker_sender)) = (sellers, broker.as_ref()) {
                match maybe_notify_lead_created(tenant_id, lookup, templates, &lead, &parsed) {
                    NotificationOutcome::Publish { topic, payload } => {
                        // M15.53 / F9 — dedupe under TTL window so
                        // a NATS redelivery of the same inbound
                        // doesn't ping the operator twice.
                        let dedup_key = crate::notification_dedup::DedupKey::new(
                            tenant_id.as_str(),
                            &lead.id.0,
                            "lead_created",
                            payload.at_ms,
                        );
                        let is_dup = dedup
                            .map(|c| c.is_duplicate(&dedup_key))
                            .unwrap_or(false);
                        if is_dup {
                            tracing::debug!(
                                target: "plugin.marketing.broker",
                                key = %dedup_key.as_str(),
                                "lead_created notification deduped"
                            );
                        } else {
                            let json_payload = serde_json::to_value(&payload)
                                .unwrap_or_else(|_| serde_json::json!({}));
                            let event = nexo_microapp_sdk::BrokerEvent::new(
                                topic.clone(),
                                "marketing.notification",
                                json_payload,
                            );
                            if let Err(e) = broker_sender.publish(&topic, event).await {
                                tracing::warn!(
                                    target: "plugin.marketing.broker",
                                    error = %e,
                                    topic = %topic,
                                    "notification publish failed (non-fatal)"
                                );
                            } else {
                                tracing::debug!(
                                    target: "plugin.marketing.broker",
                                    topic = %topic,
                                    channel = ?payload.channel,
                                    "lead_created notification published"
                                );
                            }
                        }
                    }
                    other => {
                        tracing::trace!(
                            target: "plugin.marketing.broker",
                            outcome = ?other,
                            "lead_created notification skipped"
                        );
                    }
                }
            }
            HandledOutcome::LeadCreated {
                lead_id: lead.id,
                thread_id: parsed.thread_id,
                resolver_source,
            }
        }
        Err(e) => {
            tracing::error!(
                target: "plugin.marketing.broker",
                thread_id = %parsed.thread_id,
                error = %e,
                "lead create failed"
            );
            HandledOutcome::DecodeFailed(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Arc;

    use nexo_microapp_sdk::identity::{
        open_pool, SqliteCompanyStore, SqlitePersonEmailStore, SqlitePersonStore,
    };
    use nexo_tool_meta::marketing::{AssignTarget, RuleSet};
    use serde_json::json;

    use crate::identity::adapters::display_name::DisplayNameParser;
    use crate::identity::adapters::reply_to::ReplyToReader;
    use crate::tenant::TenantId;

    async fn fresh_store(tenant: &str) -> (LeadStore, TenantId) {
        let t = TenantId::new(tenant).unwrap();
        let s = LeadStore::open(PathBuf::from(":memory:"), t.clone())
            .await
            .expect("open lead store");
        (s, t)
    }

    async fn fresh_identity() -> IdentityDeps {
        let pool = open_pool(":memory:").await.expect("open identity pool");
        // The companies store is created so the pool is migrated
        // even though this test path doesn't upsert companies.
        let _ = SqliteCompanyStore::new(pool.clone());
        let persons: Arc<dyn PersonStore> = Arc::new(SqlitePersonStore::new(pool.clone()));
        let person_emails: Arc<dyn PersonEmailStore> =
            Arc::new(SqlitePersonEmailStore::new(pool));
        let chain = Arc::new(FallbackChain::new(
            vec![
                Box::new(DisplayNameParser),
                Box::new(ReplyToReader),
            ],
            0.7,
        ));
        IdentityDeps::new(persons, person_emails, chain)
    }

    fn drop_all_router(t: &TenantId) -> LeadRouter {
        LeadRouter::new(
            t.clone(),
            RuleSet {
                tenant_id: TenantIdRef(t.as_str().into()),
                version: 0,
                rules: Vec::new(),
                default_target: AssignTarget::Drop,
            },
        )
    }

    fn unassigned_router(t: &TenantId) -> LeadRouter {
        // A router whose default fires `Seller { id: "unassigned" }`
        // so a no-rule tenant still creates leads (operator picks up
        // from the inbox manually).
        LeadRouter::new(
            t.clone(),
            RuleSet {
                tenant_id: TenantIdRef(t.as_str().into()),
                version: 0,
                rules: Vec::new(),
                default_target: AssignTarget::Seller {
                    id: SellerId("unassigned".into()),
                },
            },
        )
    }

    fn seller_lookup_with(seller: nexo_tool_meta::marketing::Seller) -> SellerLookup {
        let mut map = std::collections::HashMap::new();
        map.insert(seller.id.clone(), seller);
        std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(map))
    }

    fn pedro_seller(on_vacation: bool) -> nexo_tool_meta::marketing::Seller {
        use nexo_tool_meta::marketing::{
            Seller, SellerId, SellerNotificationSettings, TenantIdRef,
        };
        Seller {
            id: SellerId("pedro".into()),
            tenant_id: TenantIdRef("acme".into()),
            name: "pedro".into(),
            primary_email: "pedro@acme.com".into(),
            alt_emails: vec![],
            signature_text: String::new(),
            working_hours: None,
            on_vacation,
            vacation_until: None,
            preferred_language: None,
            agent_id: None,
            notification_settings: Some(SellerNotificationSettings::default()),
            model_override: None,
        }
    }

    fn pedro_router(t: &TenantId) -> LeadRouter {
        LeadRouter::new(
            t.clone(),
            RuleSet {
                tenant_id: TenantIdRef(t.as_str().into()),
                version: 0,
                rules: Vec::new(),
                default_target: AssignTarget::Seller {
                    id: SellerId("pedro".into()),
                },
            },
        )
    }

    fn parsed_minimal() -> ParsedInbound {
        ParsedInbound {
            instance: "default".into(),
            account_id: "ventas".into(),
            uid: 1,
            from_email: "cliente@empresa.com".into(),
            from_display_name: None,
            to_emails: vec![],
            reply_to: None,
            subject: "Cotización".into(),
            message_id: Some("m1@e".into()),
            in_reply_to: None,
            references: vec![],
            body_excerpt: "hola".into(),
            thread_id: "m1@e".into(),
            from_domain_kind:
                nexo_microapp_sdk::enrichment::DomainKind::Corporate,
        }
    }

    #[test]
    fn vacation_seller_falls_through_to_unassigned() {
        let tenant = TenantId::new("acme").unwrap();
        let router = pedro_router(&tenant);
        let lookup = seller_lookup_with(pedro_seller(true));
        let parsed = parsed_minimal();
        let now = chrono::Utc::now();
        let (sid, why) =
            route_inbound(&parsed, &router, Some(&lookup), now).unwrap();
        assert_eq!(sid.as_ref().map(|s| s.0.as_str()), Some("unassigned"));
        assert!(
            why.iter().any(|w| w.starts_with("seller:pedro:on_vacation")),
            "missing vacation reason in audit: {why:?}",
        );
        assert!(
            why.iter()
                .any(|w| w == "availability_fallback_unassigned"),
            "missing fallback marker: {why:?}",
        );
    }

    #[test]
    fn available_seller_keeps_assignment() {
        let tenant = TenantId::new("acme").unwrap();
        let router = pedro_router(&tenant);
        // No working_hours + not on vacation ⇒ always available.
        let lookup = seller_lookup_with(pedro_seller(false));
        let parsed = parsed_minimal();
        let now = chrono::Utc::now();
        let (sid, why) =
            route_inbound(&parsed, &router, Some(&lookup), now).unwrap();
        assert_eq!(sid.as_ref().map(|s| s.0.as_str()), Some("pedro"));
        assert!(
            !why.iter()
                .any(|w| w.starts_with("availability_fallback")),
            "should not have triggered fallback: {why:?}",
        );
    }

    #[test]
    fn unknown_seller_lookup_miss_keeps_assignment() {
        // Operator removed `pedro` after authoring the rule —
        // gate has no record to evaluate, defaults to "available"
        // so the lead still threads through.
        let tenant = TenantId::new("acme").unwrap();
        let router = pedro_router(&tenant);
        let empty: SellerLookup = std::sync::Arc::new(
            arc_swap::ArcSwap::from_pointee(std::collections::HashMap::new()),
        );
        let parsed = parsed_minimal();
        let now = chrono::Utc::now();
        let (sid, _) =
            route_inbound(&parsed, &router, Some(&empty), now).unwrap();
        assert_eq!(sid.as_ref().map(|s| s.0.as_str()), Some("pedro"));
    }

    #[test]
    fn no_seller_lookup_keeps_assignment() {
        // Backwards-compat: tests + minimal setups skip the
        // lookup; gate is not enforced.
        let tenant = TenantId::new("acme").unwrap();
        let router = pedro_router(&tenant);
        let parsed = parsed_minimal();
        let now = chrono::Utc::now();
        let (sid, _) =
            route_inbound(&parsed, &router, None, now).unwrap();
        assert_eq!(sid.as_ref().map(|s| s.0.as_str()), Some("pedro"));
    }

    fn raw_email() -> Vec<u8> {
        b"From: =?utf-8?Q?Cliente?= <cliente@empresa.com>\r\n\
          To: ventas@miempresa.com\r\n\
          Subject: Cotizaci\xc3\xb3n CRM\r\n\
          Message-Id: <abc123@empresa.com>\r\n\
          Date: Fri, 1 May 2026 10:00:00 +0000\r\n\
          \r\n\
          Hola, queremos cotizar.\r\n"
            .to_vec()
    }

    #[tokio::test]
    async fn off_topic_event_is_skipped() {
        let (store, tenant) = fresh_store("acme").await;
        let router = unassigned_router(&tenant);
        let identity = fresh_identity().await;
        let out = handle_inbound_event(
            "plugin.outbound.whatsapp.ack",
            json!({}),
            &tenant,
            &store,
            Some(&router),
            Some(&identity),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert_eq!(out, HandledOutcome::Skipped);
    }

    #[tokio::test]
    async fn malformed_payload_returns_malformed() {
        let (store, tenant) = fresh_store("acme").await;
        let router = unassigned_router(&tenant);
        let identity = fresh_identity().await;
        let out = handle_inbound_event(
            "plugin.inbound.email.acme",
            json!({ "no_fields": true }),
            &tenant,
            &store,
            Some(&router),
            Some(&identity),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert_eq!(out, HandledOutcome::Malformed);
    }

    #[tokio::test]
    async fn cold_thread_creates_lead_with_real_person_id() {
        let (store, tenant) = fresh_store("acme").await;
        let router = unassigned_router(&tenant);
        let identity = fresh_identity().await;
        let payload = json!({
            "account_id": "acct-1",
            "instance": "default",
            "uid": 42_u32,
            "raw_bytes": raw_email(),
        });
        let out = handle_inbound_event(
            "plugin.inbound.email.default",
            payload,
            &tenant,
            &store,
            Some(&router),
            Some(&identity),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        let HandledOutcome::LeadCreated {
            lead_id,
            resolver_source,
            ..
        } = out
        else {
            panic!("expected LeadCreated, got {out:?}");
        };
        // Person should be persisted with deterministic id.
        let owner = identity
            .person_emails
            .find_owner("acme", "cliente@empresa.com")
            .await
            .expect("person_emails query");
        assert!(
            owner.is_some(),
            "person_email link missing after lead create"
        );
        let pid = owner.unwrap();
        assert!(pid.0.starts_with("placeholder-"));

        // why_routed carries the resolver source.
        let lead = store.get(&lead_id).await.unwrap().expect("lead row");
        assert_eq!(
            lead.seller_id.0, "unassigned",
            "default_target should pin to unassigned"
        );
        assert!(
            lead.why_routed
                .iter()
                .any(|s| s.starts_with("resolver:")),
            "why_routed missing resolver: tag → {:?}",
            lead.why_routed
        );
        assert!(
            !resolver_source.is_empty(),
            "resolver_source label must be populated"
        );
    }

    #[tokio::test]
    async fn rule_drops_inbound_when_default_target_is_drop() {
        let (store, tenant) = fresh_store("acme").await;
        let router = drop_all_router(&tenant);
        let identity = fresh_identity().await;
        let payload = json!({
            "account_id": "acct-1",
            "instance": "default",
            "uid": 42_u32,
            "raw_bytes": raw_email(),
        });
        let out = handle_inbound_event(
            "plugin.inbound.email.default",
            payload,
            &tenant,
            &store,
            Some(&router),
            Some(&identity),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            matches!(out, HandledOutcome::DroppedByRule { .. }),
            "expected DroppedByRule, got {out:?}"
        );
        // No lead should have been written.
        assert_eq!(
            store.count_by_state(nexo_tool_meta::marketing::LeadState::Cold).await.unwrap(),
            0,
            "drop rule should not create a lead row"
        );
    }

    #[tokio::test]
    async fn second_inbound_on_same_thread_updates_existing_lead() {
        let (store, tenant) = fresh_store("acme").await;
        let router = unassigned_router(&tenant);
        let identity = fresh_identity().await;
        let payload = json!({
            "account_id": "acct-1",
            "instance": "default",
            "uid": 42_u32,
            "raw_bytes": raw_email(),
        });
        let first = handle_inbound_event(
            "plugin.inbound.email.default",
            payload.clone(),
            &tenant,
            &store,
            Some(&router),
            Some(&identity),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        let HandledOutcome::LeadCreated {
            lead_id, thread_id, ..
        } = first
        else {
            panic!("first call should create lead, got {first:?}");
        };
        let second = handle_inbound_event(
            "plugin.inbound.email.default",
            payload,
            &tenant,
            &store,
            Some(&router),
            Some(&identity),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        match second {
            HandledOutcome::LeadUpdated {
                lead_id: l2,
                thread_id: t2,
            } => {
                assert_eq!(l2.0, lead_id.0);
                assert_eq!(t2, thread_id);
            }
            other => panic!("expected LeadUpdated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn placeholder_path_works_when_identity_disabled() {
        // Tests that the optional identity / router path keeps
        // the broker hop functional during tests / minimal setups.
        let (store, tenant) = fresh_store("acme").await;
        let payload = json!({
            "account_id": "acct-1",
            "instance": "default",
            "uid": 99_u32,
            "raw_bytes": raw_email(),
        });
        let out = handle_inbound_event(
            "plugin.inbound.email.default",
            payload,
            &tenant,
            &store,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            matches!(out, HandledOutcome::LeadCreated { .. }),
            "expected LeadCreated even without identity/router, got {out:?}"
        );
    }

    #[tokio::test]
    async fn cold_thread_publishes_created_event_to_bus() {
        // Wire a firehose bus and assert the Created frame
        // lands on the subscriber. End-to-end check that the
        // broker hop actually writes to the bus.
        let (store, tenant) = fresh_store("acme").await;
        let router = unassigned_router(&tenant);
        let identity = fresh_identity().await;
        let bus = LeadEventBus::new();
        let mut rx = bus.subscribe();
        let payload = json!({
            "account_id": "acct-1",
            "instance": "default",
            "uid": 7_u32,
            "raw_bytes": raw_email(),
        });
        let out = handle_inbound_event(
            "plugin.inbound.email.default",
            payload,
            &tenant,
            &store,
            Some(&router),
            Some(&identity),
            Some(&bus),
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(matches!(out, HandledOutcome::LeadCreated { .. }));
        let frame = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("frame should arrive within 200ms")
            .expect("recv ok");
        match frame {
            LeadFirehoseEvent::Created {
                tenant_id,
                from_email,
                state,
                ..
            } => {
                assert_eq!(tenant_id.0, "acme");
                assert_eq!(from_email, "cliente@empresa.com");
                assert_eq!(state, nexo_tool_meta::marketing::LeadState::Cold);
            }
            other => panic!("expected Created, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn second_inbound_publishes_thread_bumped_event() {
        let (store, tenant) = fresh_store("acme").await;
        let router = unassigned_router(&tenant);
        let identity = fresh_identity().await;
        let bus = LeadEventBus::new();
        let mut rx = bus.subscribe();
        let payload = json!({
            "account_id": "acct-1",
            "instance": "default",
            "uid": 7_u32,
            "raw_bytes": raw_email(),
        });
        // Prime: first inbound creates the lead → Created frame.
        let _ = handle_inbound_event(
            "plugin.inbound.email.default",
            payload.clone(),
            &tenant,
            &store,
            Some(&router),
            Some(&identity),
            Some(&bus),
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        let _consume_created = rx.recv().await;
        // Second inbound on the same thread → ThreadBumped frame.
        let _ = handle_inbound_event(
            "plugin.inbound.email.default",
            payload,
            &tenant,
            &store,
            Some(&router),
            Some(&identity),
            Some(&bus),
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        let bumped = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("frame should arrive")
            .expect("recv ok");
        assert!(
            matches!(bumped, LeadFirehoseEvent::ThreadBumped { .. }),
            "expected ThreadBumped, got {bumped:?}"
        );
    }
}

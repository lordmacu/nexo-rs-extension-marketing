//! WhatsApp inbound ingest (M15.23.e WA half).
//!
//! **F29 sweep:** marketing-specific by design. The
//! subscriber pipeline is the consumer of SDK primitives
//! (JID parser, identity stores, LID-PN mapping) — that's
//! the right layering. `deterministic_person_id` is 6 LOC
//! of uuid5; not worth a feature flag.
//!
//! Subscribes to `plugin.inbound.whatsapp.*` events
//! published by `nexo-plugin-whatsapp`. We DON'T process
//! the message body — that's the agent runtime's job —
//! we only extract the sender's JID, normalise it via
//! [`nexo_microapp_sdk::identity::parse_jid`], and upsert
//! a `Person` + `PersonPhone` row so the M15.23.e
//! duplicate-person matcher (in
//! [`crate::duplicate::find_duplicate_candidates`]) sees
//! WA contacts when scanning email-resolved leads.
//!
//! ## Tenant resolution
//!
//! The WA plugin's `account_id` is the WA instance label
//! (`acme-personal`, `globex-sales`, …). The marketing
//! extension's [`crate::broker::TenantResolver`] already
//! handles this for the email side; the same trait object
//! is reused here so the operator's tenant-mapping config
//! works transparently for both channels.
//!
//! ## What the WA plugin publishes
//!
//! `nexo_plugin_whatsapp::events::InboundEvent`:
//!
//! ```json
//! { "kind": "message", "from": "573001234567@s.whatsapp.net",
//!   "chat": "...", "text": "...", "is_group": false,
//!   "timestamp": 1730000000, "msg_id": "..." }
//! ```
//!
//! We accept the `Message` variant only; everything else
//! (`Connected`, `MediaReceived`, `Qr`, …) is ignored at
//! this layer. Group messages skip the upsert because
//! `is_user()` rejects `g.us`.

use std::sync::Arc;

use chrono::Utc;
use nexo_microapp_sdk::identity::{
    parse_jid, LidPnMappingStore, ParsedJid, PersonPhoneStore, PersonStore,
};
use nexo_tool_meta::marketing::{EnrichmentStatus, Person, PersonId, TenantIdRef};
use serde::Deserialize;
use uuid::Uuid;

use crate::audit::AuditLog;
use crate::tenant::TenantId;

/// Wire-shape mirror of `nexo_plugin_whatsapp::events::InboundEvent`'s
/// discriminator field. Decoded first so non-`message`
/// variants (Qr / Connected / MediaReceived / …) skip the
/// expensive full-payload decode.
#[derive(Debug, Deserialize)]
struct WaKindOnly {
    /// `"message"` is the only variant we ingest. Everything
    /// else returns [`IngestOutcome::NonMessageEvent`].
    kind: String,
}

/// Full message payload — populated only when the
/// discriminator matched. Fields beyond `from` + `account_id`
/// are agent-runtime concerns (text / media / replies) the
/// duplicate matcher doesn't need.
#[derive(Debug, Deserialize)]
struct WaMessageWire {
    /// Sender JID — `573001234567@s.whatsapp.net` for PN
    /// users, `123456@lid` for LID users, `123-456@g.us`
    /// for groups.
    from: String,
    /// `account_id` from the broker envelope. We derive the
    /// WA instance label from the topic suffix instead, but
    /// the plugin includes this so multi-account deployments
    /// can disambiguate.
    #[serde(default)]
    account_id: Option<String>,
}

/// What the ingest decided. Returned so the broker loop's
/// telemetry can count successful upserts vs skipped
/// non-user JIDs vs malformed payloads.
#[derive(Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    /// Topic prefix didn't match — not for us.
    Skipped,
    /// Wire decode failed (malformed payload). Logged at warn.
    Malformed,
    /// Event variant we don't process (Connected / Qr / …).
    NonMessageEvent,
    /// JID belonged to a non-user namespace (group /
    /// broadcast / bot / unknown server).
    NonUserJid(String),
    /// Tenant lookup failed for the WA instance.
    TenantUnresolved(String),
    /// Upsert succeeded — Person + PersonPhone landed.
    /// `person_id` is the deterministic uuid5 we minted
    /// from the canonical JID.
    Upserted {
        person_id: PersonId,
        canonical_jid: String,
        tenant_id: String,
    },
}

/// Deterministic person id from a canonical JID. Mirrors
/// the email side's `placeholder-<uuid5>` convention so
/// re-ingesting the same contact never creates a duplicate.
pub fn deterministic_person_id(canonical_jid: &str) -> PersonId {
    let ns = Uuid::NAMESPACE_DNS;
    let v5 = Uuid::new_v5(&ns, canonical_jid.to_ascii_lowercase().as_bytes());
    PersonId(format!("placeholder-{v5}"))
}

/// Process one `plugin.inbound.whatsapp.*` event.
///
/// `topic` discriminates against other broker prefixes the
/// extension also subscribes to (`plugin.inbound.email.*`,
/// `agent.email.notification.*`); off-topic events return
/// [`IngestOutcome::Skipped`].
///
/// Failures degrade silently — every variant of
/// [`IngestOutcome`] is non-fatal. The broker loop logs +
/// continues.
pub async fn handle_inbound_whatsapp_event(
    topic: &str,
    payload: serde_json::Value,
    persons: Arc<dyn PersonStore>,
    person_phones: Arc<dyn PersonPhoneStore>,
    lid_pn_mappings: Option<Arc<dyn LidPnMappingStore>>,
    tenant_resolver: &dyn crate::broker::TenantResolver,
    _audit: Option<&AuditLog>,
) -> IngestOutcome {
    if !topic.starts_with("plugin.inbound.whatsapp.")
        && topic != "plugin.inbound.whatsapp"
    {
        return IngestOutcome::Skipped;
    }

    // The WA plugin uses tagged-union serialisation
    // (`#[serde(tag = "kind")]`). We read the discriminant
    // first; `message` is the only variant the duplicate
    // matcher needs. Other variants (Qr / Connected /
    // Disconnected / MediaReceived) skip without trying to
    // decode their distinct payload shapes.
    let kind_only: WaKindOnly = match serde_json::from_value(payload.clone()) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(
                target: "marketing.whatsapp_ingest",
                topic, error = %e,
                "malformed plugin.inbound.whatsapp payload (no kind)"
            );
            return IngestOutcome::Malformed;
        }
    };
    if kind_only.kind != "message" {
        return IngestOutcome::NonMessageEvent;
    }
    let wire: WaMessageWire = match serde_json::from_value(payload) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                target: "marketing.whatsapp_ingest",
                topic, error = %e,
                "malformed plugin.inbound.whatsapp message payload"
            );
            return IngestOutcome::Malformed;
        }
    };

    // Parse + canonicalise the sender JID. Groups, status,
    // bots reject at `is_user()` — we only ingest human
    // contacts.
    let parsed = match parse_jid(&wire.from) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "marketing.whatsapp_ingest",
                topic, error = %e, from = %wire.from,
                "JID parse failed (skipping)"
            );
            return IngestOutcome::NonUserJid(wire.from);
        }
    };
    if !parsed.is_user() {
        return IngestOutcome::NonUserJid(wire.from);
    }
    let canonical = parsed.canonical();

    // Resolve tenant from the WA instance label (topic
    // suffix or the wire's account_id, whichever the broker
    // envelope carried). Mirrors the email subscriber's
    // resolver chain.
    let instance_label = topic
        .strip_prefix("plugin.inbound.whatsapp.")
        .unwrap_or("default");
    let account_id = wire
        .account_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(instance_label);
    let tenant = match tenant_resolver
        .resolve_for_account(account_id)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                target: "marketing.whatsapp_ingest",
                topic, error = %e, account_id,
                "tenant resolution failed (skipping)"
            );
            return IngestOutcome::TenantUnresolved(account_id.to_string());
        }
    };

    // F23 — when the LID-PN mapping store has a record for
    // this inbound's namespace counterpart, derive the
    // person id from the canonical PN form so a LID inbound
    // and its paired PN inbound land on the SAME Person row.
    // Without a mapping, fall back to the inbound's own
    // canonical (pre-F23 behaviour). The PersonPhone row is
    // still keyed by the inbound's canonical so direct
    // lookups by JID resolve.
    let identity_canonical = bridge_identity_via_lid_pn_mapping(
        &parsed,
        lid_pn_mappings.as_deref(),
        tenant.as_str(),
    )
    .await
    .unwrap_or_else(|| canonical.clone());

    // Upsert Person — deterministic uuid5 over the
    // canonical JID so re-ingesting the same contact never
    // forks the row. We don't have a name yet (push_name
    // is in the InboundEvent but not in our wire shape;
    // promoting that is a follow-up).
    let person = build_person_from_jid(&tenant, &identity_canonical);
    if let Err(e) = persons.upsert(tenant.as_str(), &person).await {
        tracing::warn!(
            target: "marketing.whatsapp_ingest",
            error = %e, person_id = %person.id.0,
            "PersonStore.upsert failed (skipping phone link)"
        );
        return IngestOutcome::Upserted {
            person_id: person.id.clone(),
            canonical_jid: canonical,
            tenant_id: tenant.as_str().to_string(),
        };
    }
    if let Err(e) = person_phones
        .add(tenant.as_str(), &person.id, &canonical, false)
        .await
    {
        tracing::warn!(
            target: "marketing.whatsapp_ingest",
            error = %e, person_id = %person.id.0, jid = %canonical,
            "PersonPhoneStore.add failed (non-fatal)"
        );
    }
    IngestOutcome::Upserted {
        person_id: person.id,
        canonical_jid: canonical,
        tenant_id: tenant.as_str().to_string(),
    }
}

/// F23 — when a LID inbound has a paired PN in the mapping
/// store (or vice versa), return the canonical form of the
/// "primary" namespace so both inbounds collapse onto the
/// same Person row. PN is the primary anchor — it's the
/// human's real phone number.
///
/// Returns `None` when no mapping store is wired or no
/// pair exists; caller falls back to the inbound JID's own
/// canonical.
async fn bridge_identity_via_lid_pn_mapping(
    parsed: &ParsedJid,
    store: Option<&dyn LidPnMappingStore>,
    tenant_id: &str,
) -> Option<String> {
    let store = store?;
    match parsed.server.as_str() {
        "lid" => {
            // LID inbound — look up the paired PN.
            match store.get_pn_for_lid(tenant_id, &parsed.user).await {
                Ok(Some(pn_user)) => {
                    Some(format!("{pn_user}@s.whatsapp.net"))
                }
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(
                        target: "marketing.whatsapp_ingest",
                        error = %e, lid = %parsed.user,
                        "lid_pn_mapping.get_pn_for_lid failed (skipping bridge)"
                    );
                    None
                }
            }
        }
        // PN inbound — already the canonical anchor; no
        // bridge needed (LID inbounds will collapse to the
        // PN when their mapping fires).
        _ => None,
    }
}

fn build_person_from_jid(tenant: &TenantId, canonical_jid: &str) -> Person {
    let now_ms = Utc::now().timestamp_millis();
    Person {
        id: deterministic_person_id(canonical_jid),
        tenant_id: TenantIdRef(tenant.as_str().into()),
        // No name yet — WA push_name lives on the original
        // event payload but isn't in our wire shape. Use the
        // canonical JID as a placeholder; later upserts can
        // rename via the existing identity flow.
        primary_name: canonical_jid.to_string(),
        // Empty primary_email — this person was learned via
        // WA. The duplicate matcher's email signal won't
        // fire on this row, but the phone signal will when
        // an email-side lead arrives carrying the same
        // contact.
        primary_email: String::new(),
        alt_emails: Vec::new(),
        company_id: None,
        enrichment_status: EnrichmentStatus::None,
        enrichment_confidence: 0.0,
        tags: Vec::new(),
        created_at_ms: now_ms,
        last_seen_at_ms: now_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use nexo_microapp_sdk::identity::{IdentityError, PersonPhone};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakePersonStore {
        rows: Mutex<HashMap<(String, String), Person>>,
    }
    #[async_trait]
    impl PersonStore for FakePersonStore {
        async fn upsert(
            &self,
            tenant: &str,
            p: &Person,
        ) -> Result<Person, IdentityError> {
            self.rows.lock().unwrap().insert(
                (tenant.to_string(), p.id.0.clone()),
                p.clone(),
            );
            Ok(p.clone())
        }
        async fn get(
            &self,
            tenant: &str,
            id: &PersonId,
        ) -> Result<Option<Person>, IdentityError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .get(&(tenant.to_string(), id.0.clone()))
                .cloned())
        }
        async fn find_by_email(
            &self,
            _t: &str,
            _e: &str,
        ) -> Result<Option<Person>, IdentityError> {
            Ok(None)
        }
        async fn delete_by_tenant(
            &self,
            _t: &str,
        ) -> Result<u64, IdentityError> {
            Ok(0)
        }
    }

    #[derive(Default)]
    struct FakePhoneStore {
        rows: Mutex<HashMap<(String, String), PersonId>>,
    }
    #[async_trait]
    impl PersonPhoneStore for FakePhoneStore {
        async fn add(
            &self,
            tenant: &str,
            person: &PersonId,
            phone: &str,
            _v: bool,
        ) -> Result<PersonPhone, IdentityError> {
            self.rows.lock().unwrap().insert(
                (tenant.to_string(), phone.to_string()),
                person.clone(),
            );
            Ok(PersonPhone {
                person_id: person.clone(),
                tenant_id: TenantIdRef(tenant.into()),
                phone: phone.into(),
                verified: false,
                added_at_ms: 0,
            })
        }
        async fn list_for_person(
            &self,
            _t: &str,
            _p: &PersonId,
        ) -> Result<Vec<PersonPhone>, IdentityError> {
            Ok(vec![])
        }
        async fn find_owner(
            &self,
            tenant: &str,
            phone: &str,
        ) -> Result<Option<PersonId>, IdentityError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .get(&(tenant.to_string(), phone.to_string()))
                .cloned())
        }
        async fn delete_by_tenant(
            &self,
            _t: &str,
        ) -> Result<u64, IdentityError> {
            Ok(0)
        }
    }

    fn fixture_resolver() -> crate::broker::StaticTenantResolver {
        crate::broker::StaticTenantResolver::new([(
            "acme-personal".to_string(),
            TenantId::new("acme").unwrap(),
        )])
        .with_default(TenantId::new("acme").unwrap())
    }

    fn message_payload(from: &str) -> serde_json::Value {
        json!({
            "kind": "message",
            "from": from,
            "chat": from,
            "text": "Hola",
            "is_group": false,
            "timestamp": 1_730_000_000_i64,
            "msg_id": "msg-1",
            "account_id": "acme-personal",
        })
    }

    #[tokio::test]
    async fn off_topic_event_is_skipped() {
        let persons: Arc<dyn PersonStore> = Arc::new(FakePersonStore::default());
        let phones: Arc<dyn PersonPhoneStore> = Arc::new(FakePhoneStore::default());
        let resolver = fixture_resolver();
        let out = handle_inbound_whatsapp_event(
            "plugin.inbound.email.acme",
            json!({}),
            persons,
            phones,
            None,
            &resolver,
            None,
        )
        .await;
        assert_eq!(out, IngestOutcome::Skipped);
    }

    #[tokio::test]
    async fn upsert_happy_path_persists_person_and_phone() {
        let persons = Arc::new(FakePersonStore::default());
        let phones = Arc::new(FakePhoneStore::default());
        let resolver = fixture_resolver();
        let out = handle_inbound_whatsapp_event(
            "plugin.inbound.whatsapp.acme-personal",
            message_payload("573001234567@s.whatsapp.net"),
            persons.clone() as Arc<dyn PersonStore>,
            phones.clone() as Arc<dyn PersonPhoneStore>,
            None,
            &resolver,
            None,
        )
        .await;
        let IngestOutcome::Upserted {
            person_id,
            canonical_jid,
            tenant_id,
        } = out
        else {
            panic!("expected Upserted, got {out:?}");
        };
        assert_eq!(canonical_jid, "573001234567@s.whatsapp.net");
        assert_eq!(tenant_id, "acme");
        // Phone row threaded back to the same person id.
        let owner = phones
            .find_owner("acme", "573001234567@s.whatsapp.net")
            .await
            .unwrap();
        assert_eq!(owner, Some(person_id));
    }

    #[tokio::test]
    async fn deterministic_person_id_collapses_re_ingest() {
        let persons = Arc::new(FakePersonStore::default());
        let phones = Arc::new(FakePhoneStore::default());
        let resolver = fixture_resolver();
        // Same JID twice — re-ingest must NOT create a
        // second person row.
        for _ in 0..2 {
            let _ = handle_inbound_whatsapp_event(
                "plugin.inbound.whatsapp.acme-personal",
                message_payload("573001234567@s.whatsapp.net"),
                persons.clone() as Arc<dyn PersonStore>,
                phones.clone() as Arc<dyn PersonPhoneStore>,
                None,
            &resolver,
                None,
            )
            .await;
        }
        let row_count = persons.rows.lock().unwrap().len();
        assert_eq!(row_count, 1, "deterministic id should collapse");
    }

    #[tokio::test]
    async fn legacy_c_us_canonicalises_to_s_whatsapp_net() {
        // Same human; one inbound on legacy `c.us`, one on
        // canonical `s.whatsapp.net`. Both should land on
        // the same person + same phone row.
        let persons = Arc::new(FakePersonStore::default());
        let phones = Arc::new(FakePhoneStore::default());
        let resolver = fixture_resolver();
        for jid in &["573001234567@c.us", "573001234567@s.whatsapp.net"] {
            let _ = handle_inbound_whatsapp_event(
                "plugin.inbound.whatsapp.acme-personal",
                message_payload(jid),
                persons.clone() as Arc<dyn PersonStore>,
                phones.clone() as Arc<dyn PersonPhoneStore>,
                None,
            &resolver,
                None,
            )
            .await;
        }
        // Single phone row keyed by the canonical form.
        let owner = phones
            .find_owner("acme", "573001234567@s.whatsapp.net")
            .await
            .unwrap();
        assert!(owner.is_some());
        let person_count = persons.rows.lock().unwrap().len();
        assert_eq!(person_count, 1);
    }

    #[tokio::test]
    async fn group_jid_skips_upsert() {
        let persons = Arc::new(FakePersonStore::default());
        let phones = Arc::new(FakePhoneStore::default());
        let resolver = fixture_resolver();
        let out = handle_inbound_whatsapp_event(
            "plugin.inbound.whatsapp.acme-personal",
            message_payload("12345-67890@g.us"),
            persons.clone() as Arc<dyn PersonStore>,
            phones.clone() as Arc<dyn PersonPhoneStore>,
            None,
            &resolver,
            None,
        )
        .await;
        assert!(matches!(out, IngestOutcome::NonUserJid(_)));
        assert!(persons.rows.lock().unwrap().is_empty());
        assert!(phones.rows.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn non_message_kind_is_filtered() {
        let persons = Arc::new(FakePersonStore::default());
        let phones = Arc::new(FakePhoneStore::default());
        let resolver = fixture_resolver();
        let payload = json!({
            "kind": "qr",
            "ascii": "...",
            "png_base64": "...",
            "expires_at": 0,
        });
        let out = handle_inbound_whatsapp_event(
            "plugin.inbound.whatsapp.acme-personal",
            payload,
            persons.clone() as Arc<dyn PersonStore>,
            phones.clone() as Arc<dyn PersonPhoneStore>,
            None,
            &resolver,
            None,
        )
        .await;
        assert_eq!(out, IngestOutcome::NonMessageEvent);
    }

    #[tokio::test]
    async fn malformed_payload_returns_typed_outcome() {
        let persons: Arc<dyn PersonStore> = Arc::new(FakePersonStore::default());
        let phones: Arc<dyn PersonPhoneStore> = Arc::new(FakePhoneStore::default());
        let resolver = fixture_resolver();
        let out = handle_inbound_whatsapp_event(
            "plugin.inbound.whatsapp.acme-personal",
            json!({ "no_kind": true }),
            persons,
            phones,
            None,
            &resolver,
            None,
        )
        .await;
        assert_eq!(out, IngestOutcome::Malformed);
    }

    #[tokio::test]
    async fn lid_jid_kept_distinct_from_pn() {
        // Same human's LID + PN both land — but different
        // canonical forms ⇒ different deterministic ids ⇒
        // two person rows. Bridging requires the
        // forthcoming `LidPnMappingStore` (F23). Until then,
        // each namespace is its own identity.
        let persons = Arc::new(FakePersonStore::default());
        let phones = Arc::new(FakePhoneStore::default());
        let resolver = fixture_resolver();
        for jid in &["573001234567@s.whatsapp.net", "abcdef@lid"] {
            let _ = handle_inbound_whatsapp_event(
                "plugin.inbound.whatsapp.acme-personal",
                message_payload(jid),
                persons.clone() as Arc<dyn PersonStore>,
                phones.clone() as Arc<dyn PersonPhoneStore>,
                None,
            &resolver,
                None,
            )
            .await;
        }
        let count = persons.rows.lock().unwrap().len();
        assert_eq!(count, 2, "PN and LID stay distinct without a mapping store");
    }

    #[tokio::test]
    async fn deterministic_id_function_stable() {
        // Pure-function sanity — same input always returns
        // same id across calls, machines, restarts.
        let a = deterministic_person_id("573001234567@s.whatsapp.net");
        let b = deterministic_person_id("573001234567@s.whatsapp.net");
        assert_eq!(a, b);
        // Case-insensitive on the input — `Uuid::new_v5`
        // is byte-sensitive but we lower-case before hashing.
        let c = deterministic_person_id("573001234567@S.WHATSAPP.NET");
        assert_eq!(a, c);
        // Distinct JID ⇒ distinct id.
        let d = deterministic_person_id("999999999999@s.whatsapp.net");
        assert_ne!(a, d);
    }

    // ─── F23 — LID ↔ PN mapping bridge ────────────────────────

    use nexo_microapp_sdk::identity::LidPnMapping;

    #[derive(Default)]
    struct FakeLidPnStore {
        rows: Mutex<HashMap<(String, String), String>>, // (tenant, lid) -> pn
    }

    #[async_trait]
    impl LidPnMappingStore for FakeLidPnStore {
        async fn put(
            &self,
            tenant: &str,
            lid_user: &str,
            pn_user: &str,
        ) -> Result<LidPnMapping, IdentityError> {
            self.rows.lock().unwrap().insert(
                (tenant.to_string(), lid_user.to_string()),
                pn_user.to_string(),
            );
            Ok(LidPnMapping {
                tenant_id: TenantIdRef(tenant.into()),
                lid_user: lid_user.into(),
                pn_user: pn_user.into(),
                observed_at_ms: 0,
            })
        }
        async fn get_pn_for_lid(
            &self,
            tenant: &str,
            lid_user: &str,
        ) -> Result<Option<String>, IdentityError> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .get(&(tenant.to_string(), lid_user.to_string()))
                .cloned())
        }
        async fn get_lid_for_pn(
            &self,
            _tenant: &str,
            _pn_user: &str,
        ) -> Result<Option<String>, IdentityError> {
            Ok(None)
        }
        async fn delete_by_tenant(
            &self,
            _t: &str,
        ) -> Result<u64, IdentityError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn lid_inbound_with_mapping_collapses_to_pn_person_id() {
        // Given: a LID ↔ PN mapping recorded for this tenant.
        // When: a LID inbound lands, the deterministic
        // person_id should match the PN canonical form's id,
        // NOT the LID's. Result: a future PN inbound from the
        // same human collapses onto the same Person row.
        let persons = Arc::new(FakePersonStore::default());
        let phones = Arc::new(FakePhoneStore::default());
        let lid_pn = Arc::new(FakeLidPnStore::default());
        let resolver = fixture_resolver();
        // Operator-known mapping: LID `123456789` ↔ PN
        // `573001234567`.
        lid_pn.put("acme", "123456789", "573001234567")
            .await
            .unwrap();
        let mapping_arc: Arc<dyn LidPnMappingStore> = lid_pn.clone();

        let out = handle_inbound_whatsapp_event(
            "plugin.inbound.whatsapp.acme-personal",
            message_payload("123456789@lid"),
            persons.clone() as Arc<dyn PersonStore>,
            phones.clone() as Arc<dyn PersonPhoneStore>,
            Some(mapping_arc),
            &resolver,
            None,
        )
        .await;
        let IngestOutcome::Upserted { person_id, .. } = out else {
            panic!("expected Upserted, got {out:?}");
        };
        // Person id collapsed to the PN canonical form.
        let pn_canonical = "573001234567@s.whatsapp.net";
        assert_eq!(person_id, deterministic_person_id(pn_canonical));
    }

    #[tokio::test]
    async fn lid_inbound_without_mapping_keeps_own_identity() {
        // No mapping store wired ⇒ pre-F23 behaviour. LID
        // becomes its own identity.
        let persons = Arc::new(FakePersonStore::default());
        let phones = Arc::new(FakePhoneStore::default());
        let resolver = fixture_resolver();
        let out = handle_inbound_whatsapp_event(
            "plugin.inbound.whatsapp.acme-personal",
            message_payload("123456789@lid"),
            persons.clone() as Arc<dyn PersonStore>,
            phones.clone() as Arc<dyn PersonPhoneStore>,
            None,
            &resolver,
            None,
        )
        .await;
        let IngestOutcome::Upserted { person_id, .. } = out else {
            panic!("expected Upserted");
        };
        // Person id derived from the LID's own canonical.
        assert_eq!(
            person_id,
            deterministic_person_id("123456789@lid"),
        );
    }

    #[tokio::test]
    async fn lid_inbound_with_mapping_store_but_no_pair_keeps_own_identity() {
        // Mapping store wired but no row for THIS LID. The
        // bridge falls back to the LID's own canonical.
        let persons = Arc::new(FakePersonStore::default());
        let phones = Arc::new(FakePhoneStore::default());
        let lid_pn = Arc::new(FakeLidPnStore::default());
        let resolver = fixture_resolver();
        let mapping_arc: Arc<dyn LidPnMappingStore> = lid_pn.clone();
        let out = handle_inbound_whatsapp_event(
            "plugin.inbound.whatsapp.acme-personal",
            message_payload("999999@lid"),
            persons.clone() as Arc<dyn PersonStore>,
            phones.clone() as Arc<dyn PersonPhoneStore>,
            Some(mapping_arc),
            &resolver,
            None,
        )
        .await;
        let IngestOutcome::Upserted { person_id, .. } = out else {
            panic!("expected Upserted");
        };
        assert_eq!(
            person_id,
            deterministic_person_id("999999@lid"),
        );
    }

    #[tokio::test]
    async fn pn_inbound_does_not_consult_mapping() {
        // PN inbound is the canonical anchor — no bridge
        // needed. The store is consulted only on LID
        // inbounds.
        let persons = Arc::new(FakePersonStore::default());
        let phones = Arc::new(FakePhoneStore::default());
        let lid_pn = Arc::new(FakeLidPnStore::default());
        let resolver = fixture_resolver();
        // Even with a mapping recorded, a PN inbound's id
        // comes from the PN's own canonical.
        lid_pn.put("acme", "123456789", "573001234567")
            .await
            .unwrap();
        let mapping_arc: Arc<dyn LidPnMappingStore> = lid_pn.clone();
        let out = handle_inbound_whatsapp_event(
            "plugin.inbound.whatsapp.acme-personal",
            message_payload("573001234567@s.whatsapp.net"),
            persons.clone() as Arc<dyn PersonStore>,
            phones.clone() as Arc<dyn PersonPhoneStore>,
            Some(mapping_arc),
            &resolver,
            None,
        )
        .await;
        let IngestOutcome::Upserted { person_id, .. } = out else {
            panic!("expected Upserted");
        };
        assert_eq!(
            person_id,
            deterministic_person_id("573001234567@s.whatsapp.net"),
        );
    }

    #[tokio::test]
    async fn lid_and_pn_inbounds_collapse_when_mapping_known() {
        // Integration: same human's LID + PN both land
        // (in either order) and the mapping is known. Both
        // upserts must hit the SAME Person row.
        let persons = Arc::new(FakePersonStore::default());
        let phones = Arc::new(FakePhoneStore::default());
        let lid_pn = Arc::new(FakeLidPnStore::default());
        let resolver = fixture_resolver();
        lid_pn.put("acme", "123456789", "573001234567")
            .await
            .unwrap();
        let mapping_arc: Arc<dyn LidPnMappingStore> = lid_pn.clone();
        for jid in &["123456789@lid", "573001234567@s.whatsapp.net"] {
            let _ = handle_inbound_whatsapp_event(
                "plugin.inbound.whatsapp.acme-personal",
                message_payload(jid),
                persons.clone() as Arc<dyn PersonStore>,
                phones.clone() as Arc<dyn PersonPhoneStore>,
                Some(mapping_arc.clone()),
                &resolver,
                None,
            )
            .await;
        }
        // Single Person row — both inbounds collapsed via
        // the mapping bridge.
        let count = persons.rows.lock().unwrap().len();
        assert_eq!(count, 1, "LID + PN should collapse with mapping");
    }
}

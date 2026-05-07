//! YAML-backed config loaders for the per-tenant config files.
//!
//! Layout: `${state_root}/marketing/<tenant_id>/<config>.yaml`,
//! one file per entity type:
//!
//! - `mailboxes.yaml`     → `Vec<MailboxConfig>`
//! - `vendedores.yaml`    → `Vec<Vendedor>`
//! - `rules.yaml`         → `RuleSet` (loaded by `lead::router`)
//! - `followup_profiles.yaml` → `Vec<FollowupProfile>`
//!
//! Every loader is read-only at this milestone (M15.31 ships
//! GET endpoints; PUT lands in M15.32 once the YAML write
//! helpers + atomic-rename + reload-on-disk-change pipeline
//! settle). Missing file → empty list, NOT an error: the
//! operator's tenant might be brand-new, and the UI should
//! show an empty state rather than an exception.
//!
//! Tenant scoping is by file path — the loader takes a
//! `&TenantId` + computes the state dir via
//! `TenantId::state_dir`. Cross-tenant access is impossible by
//! construction.

use std::io::Write;
use std::path::{Path, PathBuf};

use nexo_tool_meta::marketing::{FollowupProfile, MailboxConfig, Vendedor};

use crate::error::MarketingError;
use crate::tenant::TenantId;

/// Compute the per-tenant config-file path. Public so the
/// admin handlers can stat the file for a "config exists"
/// indicator.
pub fn config_path(state_root: impl AsRef<Path>, tenant: &TenantId, name: &str) -> PathBuf {
    tenant.state_dir(state_root.as_ref()).join(name)
}

/// Generic helper: read + YAML-parse the per-tenant config
/// file. Missing file returns the empty default; parse failures
/// surface as `MarketingError::Config` so the admin layer can
/// 500 with a typed body.
fn load_yaml_list<T>(
    state_root: impl AsRef<Path>,
    tenant: &TenantId,
    name: &str,
) -> Result<Vec<T>, MarketingError>
where
    T: serde::de::DeserializeOwned,
{
    let path = config_path(state_root.as_ref(), tenant, name);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let yaml = std::fs::read_to_string(&path)
        .map_err(|e| MarketingError::Config(format!("read {}: {e}", path.display())))?;
    serde_yaml::from_str::<Vec<T>>(&yaml)
        .map_err(|e| MarketingError::Config(format!("parse {}: {e}", path.display())))
}

/// Read `mailboxes.yaml`. Missing file → empty list (operator
/// hasn't configured any mailboxes yet).
pub fn load_mailboxes(
    state_root: impl AsRef<Path>,
    tenant: &TenantId,
) -> Result<Vec<MailboxConfig>, MarketingError> {
    load_yaml_list(state_root, tenant, "mailboxes.yaml")
}

/// Read `vendedores.yaml`. Missing file → empty list.
pub fn load_vendedores(
    state_root: impl AsRef<Path>,
    tenant: &TenantId,
) -> Result<Vec<Vendedor>, MarketingError> {
    load_yaml_list(state_root, tenant, "vendedores.yaml")
}

/// Read `followup_profiles.yaml`. Missing file → empty list.
pub fn load_followup_profiles(
    state_root: impl AsRef<Path>,
    tenant: &TenantId,
) -> Result<Vec<FollowupProfile>, MarketingError> {
    load_yaml_list(state_root, tenant, "followup_profiles.yaml")
}

/// Atomic YAML write — serialise `value` then rename a temp
/// file into place so a partial write never leaves a half-
/// rewritten config on disk. Creates the per-tenant subdir
/// (and `state_root`) if missing — operator UI's first save
/// shouldn't fail because the dir wasn't materialised.
fn write_yaml_atomic<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), MarketingError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            MarketingError::Config(format!("create {}: {e}", parent.display()))
        })?;
    }
    let yaml = serde_yaml::to_string(value)
        .map_err(|e| MarketingError::Config(format!("serialise yaml: {e}")))?;
    // tmp file in the same directory so `rename` is atomic
    // (cross-fs renames fall back to copy + unlink, losing
    // the atomicity guarantee).
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".into());
    let tmp = dir.join(format!(".{stem}.tmp"));
    let mut f = std::fs::File::create(&tmp)
        .map_err(|e| MarketingError::Config(format!("create {}: {e}", tmp.display())))?;
    f.write_all(yaml.as_bytes())
        .map_err(|e| MarketingError::Config(format!("write {}: {e}", tmp.display())))?;
    // fsync the tmp before rename so the rename can't
    // outpace the data hitting disk on a crash.
    f.sync_all()
        .map_err(|e| MarketingError::Config(format!("fsync {}: {e}", tmp.display())))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| {
        MarketingError::Config(format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    Ok(())
}

/// Generic write helper symmetric to `load_yaml_list`. Used by
/// every list-shaped config (mailboxes / vendedores /
/// followup_profiles). Caller's responsibility to validate the
/// payload before calling — we serialise as-is.
fn save_yaml_list<T: serde::Serialize>(
    state_root: impl AsRef<Path>,
    tenant: &TenantId,
    name: &str,
    value: &Vec<T>,
) -> Result<(), MarketingError> {
    let path = config_path(state_root.as_ref(), tenant, name);
    write_yaml_atomic(&path, value)
}

/// Write `mailboxes.yaml`. Replaces the entire list — caller
/// passes the post-edit `Vec<MailboxConfig>`. No diff / merge:
/// keeps the contract simple + the operator UI sends the full
/// post state.
pub fn save_mailboxes(
    state_root: impl AsRef<Path>,
    tenant: &TenantId,
    rows: &Vec<MailboxConfig>,
) -> Result<(), MarketingError> {
    save_yaml_list(state_root, tenant, "mailboxes.yaml", rows)
}

/// Write `vendedores.yaml`. Same full-replace contract.
pub fn save_vendedores(
    state_root: impl AsRef<Path>,
    tenant: &TenantId,
    rows: &Vec<Vendedor>,
) -> Result<(), MarketingError> {
    save_yaml_list(state_root, tenant, "vendedores.yaml", rows)
}

/// Write `followup_profiles.yaml`. Same full-replace contract.
pub fn save_followup_profiles(
    state_root: impl AsRef<Path>,
    tenant: &TenantId,
    rows: &Vec<FollowupProfile>,
) -> Result<(), MarketingError> {
    save_yaml_list(state_root, tenant, "followup_profiles.yaml", rows)
}

/// Write `rules.yaml`. Distinct from the list helpers because
/// the rule set is a single document (rules + default target +
/// version). The router doesn't auto-reload from disk yet —
/// the operator restarts the extension after saving rules
/// today; M15.33 wires the reload-on-change pipeline.
pub fn save_rules(
    state_root: impl AsRef<Path>,
    tenant: &TenantId,
    rule_set: &nexo_tool_meta::marketing::RuleSet,
) -> Result<(), MarketingError> {
    let path = config_path(state_root.as_ref(), tenant, "rules.yaml");
    write_yaml_atomic(&path, rule_set)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use nexo_tool_meta::marketing::{
        MailboxMode, TenantIdRef, VendedorId, WorkingHoursWindow,
    };
    use tempfile::tempdir;

    fn fresh_vendedor() -> Vendedor {
        Vendedor {
            id: VendedorId("pedro".into()),
            tenant_id: TenantIdRef("acme".into()),
            name: "Pedro García".into(),
            primary_email: "pedro@acme.com".into(),
            alt_emails: Vec::new(),
            signature_text: "—\nPedro · Account Manager".into(),
            working_hours: Some(WorkingHoursWindow {
                timezone: "America/Bogota".into(),
                mon_fri: None,
                saturday: None,
                sunday: None,
            }),
            on_vacation: false,
            vacation_until: None,
            preferred_language: Some("es".into()),
            agent_id: None,
            model_override: None,
            notification_settings: None,
        }
    }

    fn fresh_mailbox() -> MailboxConfig {
        MailboxConfig {
            id: "mb-1".into(),
            tenant_id: TenantIdRef("acme".into()),
            address: "ventas@acme.com".into(),
            provider: "gmail".into(),
            mode: MailboxMode::Adaptive,
            poll_interval_seconds: 30,
            active: true,
            draft_mode: true,
            active_hours: None,
            email_plugin_instance: "default".into(),
        }
    }

    fn tenant() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn write_yaml(state_root: &Path, name: &str, body: &str) {
        let dir = state_root.join("marketing").join("acme");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn missing_file_returns_empty_vec() {
        let tmp = tempdir().unwrap();
        assert!(load_mailboxes(tmp.path(), &tenant()).unwrap().is_empty());
        assert!(load_vendedores(tmp.path(), &tenant()).unwrap().is_empty());
        assert!(load_followup_profiles(tmp.path(), &tenant())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn vendedores_yaml_round_trips() {
        let tmp = tempdir().unwrap();
        let v = fresh_vendedor();
        write_yaml(
            tmp.path(),
            "vendedores.yaml",
            &serde_yaml::to_string(&vec![v.clone()]).unwrap(),
        );
        let got = load_vendedores(tmp.path(), &tenant()).unwrap();
        assert_eq!(got, vec![v]);
    }

    #[test]
    fn mailboxes_yaml_round_trips() {
        let tmp = tempdir().unwrap();
        let m = fresh_mailbox();
        write_yaml(
            tmp.path(),
            "mailboxes.yaml",
            &serde_yaml::to_string(&vec![m.clone()]).unwrap(),
        );
        let got = load_mailboxes(tmp.path(), &tenant()).unwrap();
        assert_eq!(got, vec![m]);
    }

    #[test]
    fn followup_profiles_yaml_round_trips() {
        let tmp = tempdir().unwrap();
        let f = FollowupProfile {
            id: "default".into(),
            cadence: vec!["24h".into(), "72h".into()],
            max_attempts: 2,
            stop_on_reply: true,
        };
        write_yaml(
            tmp.path(),
            "followup_profiles.yaml",
            &serde_yaml::to_string(&vec![f.clone()]).unwrap(),
        );
        let got = load_followup_profiles(tmp.path(), &tenant()).unwrap();
        assert_eq!(got, vec![f]);
    }

    #[test]
    fn parse_error_surfaces_typed_config_error() {
        let tmp = tempdir().unwrap();
        write_yaml(tmp.path(), "vendedores.yaml", "this is: { not: valid: yaml");
        let err = load_vendedores(tmp.path(), &tenant()).unwrap_err();
        assert!(
            matches!(err, MarketingError::Config(_)),
            "expected Config error, got {err:?}"
        );
    }

    #[test]
    fn cross_tenant_isolation_via_path() {
        let tmp = tempdir().unwrap();
        // Write under acme; lookup under globex must not see it.
        let v = fresh_vendedor();
        write_yaml(
            tmp.path(),
            "vendedores.yaml",
            &serde_yaml::to_string(&vec![v]).unwrap(),
        );
        let other = TenantId::new("globex").unwrap();
        assert!(load_vendedores(tmp.path(), &other).unwrap().is_empty());
    }

    #[test]
    fn save_then_load_round_trips_vendedores() {
        let tmp = tempdir().unwrap();
        let rows = vec![fresh_vendedor()];
        save_vendedores(tmp.path(), &tenant(), &rows).unwrap();
        let got = load_vendedores(tmp.path(), &tenant()).unwrap();
        assert_eq!(got, rows);
    }

    #[test]
    fn save_then_load_round_trips_mailboxes() {
        let tmp = tempdir().unwrap();
        let rows = vec![fresh_mailbox()];
        save_mailboxes(tmp.path(), &tenant(), &rows).unwrap();
        let got = load_mailboxes(tmp.path(), &tenant()).unwrap();
        assert_eq!(got, rows);
    }

    #[test]
    fn save_then_load_round_trips_followup_profiles() {
        let tmp = tempdir().unwrap();
        let rows = vec![FollowupProfile {
            id: "default".into(),
            cadence: vec!["24h".into(), "72h".into()],
            max_attempts: 2,
            stop_on_reply: true,
        }];
        save_followup_profiles(tmp.path(), &tenant(), &rows).unwrap();
        let got = load_followup_profiles(tmp.path(), &tenant()).unwrap();
        assert_eq!(got, rows);
    }

    #[test]
    fn save_creates_per_tenant_dir_if_missing() {
        // No dir pre-created — saving should `mkdir -p` the path.
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("marketing").join("acme");
        assert!(!dir.exists());
        save_mailboxes(tmp.path(), &tenant(), &vec![fresh_mailbox()]).unwrap();
        assert!(dir.join("mailboxes.yaml").exists());
    }

    #[test]
    fn save_overwrites_existing_file_atomically() {
        let tmp = tempdir().unwrap();
        save_vendedores(tmp.path(), &tenant(), &vec![fresh_vendedor()]).unwrap();
        // Replace with empty list — the rewrite must leave a
        // valid yaml document (not a half-written file).
        save_vendedores(tmp.path(), &tenant(), &Vec::<Vendedor>::new()).unwrap();
        let got = load_vendedores(tmp.path(), &tenant()).unwrap();
        assert!(got.is_empty());
        // No tmp file leftover.
        let path = tmp.path().join("marketing").join("acme");
        let stragglers: Vec<_> = std::fs::read_dir(&path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(stragglers.is_empty(), "tmp leftovers: {stragglers:?}");
    }

    #[test]
    fn save_rules_round_trips() {
        use nexo_tool_meta::marketing::{AssignTarget, RuleSet, VendedorId};
        let tmp = tempdir().unwrap();
        let rs = RuleSet {
            tenant_id: TenantIdRef("acme".into()),
            version: 1,
            rules: Vec::new(),
            default_target: AssignTarget::Vendedor {
                id: VendedorId("pedro".into()),
            },
        };
        save_rules(tmp.path(), &tenant(), &rs).unwrap();
        let path = tmp.path().join("marketing").join("acme").join("rules.yaml");
        let yaml = std::fs::read_to_string(&path).unwrap();
        // The router's load_rule_set will parse this back; for
        // the unit test we just sanity-check the file is non-
        // empty + carries the tenant id.
        assert!(yaml.contains("acme"));
        assert!(yaml.contains("pedro"));
    }
}

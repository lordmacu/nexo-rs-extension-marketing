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
}

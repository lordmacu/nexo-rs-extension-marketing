//! `TenantId` newtype — the per-tenant scope every persistent
//! row + every NATS subject + every admin endpoint partitions
//! by.
//!
//! Validates against the same regex the daemon's tenants
//! handler uses (`crates/tool-meta/src/admin/tenants.rs`)
//! so a string that round-trips through `nexo/admin/tenants/list`
//! is always parseable here. Wraps a `String` for ergonomics
//! but offers explicit `from_str` so silent typos surface as
//! errors at the boundary.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Validation regex — kebab-case, 2-31 chars, must start with
/// a letter. Mirrors `[a-z][a-z0-9-]{1,30}` used everywhere
/// else in the framework.
const TENANT_ID_PATTERN: &str = "^[a-z][a-z0-9-]{1,30}$";

/// Strongly-typed tenant identifier. Construct via
/// [`TenantId::new`] or [`FromStr`] so the validation runs
/// once at the boundary; downstream code can hand `&TenantId`
/// around without re-checking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(transparent)]
pub struct TenantId(String);

impl TenantId {
    /// Build a new tenant id, validating against the canonical
    /// regex. Returns the offending string on failure so the
    /// caller can surface a 400 + the bad value.
    pub fn new(s: impl Into<String>) -> Result<Self, TenantIdError> {
        let raw: String = s.into();
        if !is_valid(&raw) {
            return Err(TenantIdError::Invalid(raw));
        }
        Ok(Self(raw))
    }

    /// View the underlying string. Cheap clone-free access for
    /// SQL parameter binding + path joining.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Per-tenant directory under
    /// `${NEXO_EXTENSION_STATE_ROOT}/marketing/<tenant_id>/`.
    /// Caller is responsible for creating it; this just
    /// computes the path.
    pub fn state_dir(&self, root: impl AsRef<Path>) -> PathBuf {
        root.as_ref().join("marketing").join(&self.0)
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TenantId {
    type Err = TenantIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s.to_owned())
    }
}

/// Conversion errors surfaced from `TenantId::new` /
/// `from_str`. Kept tiny so call sites just propagate.
#[derive(Debug, thiserror::Error)]
pub enum TenantIdError {
    /// Input failed the validation regex. Carries the offending
    /// string so the caller can echo it back to the operator.
    #[error("invalid tenant id: {0:?}")]
    Invalid(String),
}

fn is_valid(s: &str) -> bool {
    static RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(TENANT_ID_PATTERN).unwrap());
    RE.is_match(s)
}

/// Bridges a `TenantIdRef` from `nexo-tool-meta` (over-the-wire
/// type, plain `String` newtype) into a validated [`TenantId`].
/// Useful at the boundary between the broker / HTTP layer and
/// the typed core.
impl TryFrom<&nexo_tool_meta::marketing::TenantIdRef> for TenantId {
    type Error = TenantIdError;
    fn try_from(value: &nexo_tool_meta::marketing::TenantIdRef) -> Result<Self, Self::Error> {
        TenantId::new(value.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn accepts_kebab_case_starting_with_letter() {
        assert!(TenantId::new("acme").is_ok());
        assert!(TenantId::new("acme-corp").is_ok());
        assert!(TenantId::new("a1").is_ok());
        assert!(TenantId::new("a-very-long-tenant-id-1234567").is_ok());
    }

    #[test]
    fn rejects_invalid_inputs() {
        // empty
        assert!(matches!(
            TenantId::new(""),
            Err(TenantIdError::Invalid(_))
        ));
        // too short — regex requires 2-31, single char fails
        assert!(matches!(
            TenantId::new("a"),
            Err(TenantIdError::Invalid(_))
        ));
        // starts with digit
        assert!(matches!(
            TenantId::new("1acme"),
            Err(TenantIdError::Invalid(_))
        ));
        // uppercase
        assert!(matches!(
            TenantId::new("Acme"),
            Err(TenantIdError::Invalid(_))
        ));
        // invalid char
        assert!(matches!(
            TenantId::new("acme_corp"),
            Err(TenantIdError::Invalid(_))
        ));
        // too long (32 chars)
        let too_long = "a".repeat(32);
        assert!(matches!(
            TenantId::new(too_long),
            Err(TenantIdError::Invalid(_))
        ));
    }

    #[test]
    fn from_str_works() {
        let t: TenantId = "acme-corp".parse().unwrap();
        assert_eq!(t.as_str(), "acme-corp");
    }

    #[test]
    fn display_matches_inner() {
        let t = TenantId::new("acme").unwrap();
        assert_eq!(format!("{t}"), "acme");
    }

    #[test]
    fn state_dir_joins_correctly() {
        let t = TenantId::new("acme").unwrap();
        let root = PathBuf::from("/state");
        assert_eq!(t.state_dir(&root), PathBuf::from("/state/marketing/acme"));
    }

    #[test]
    fn try_from_tool_meta_ref() {
        let r = nexo_tool_meta::marketing::TenantIdRef("acme".into());
        let t: TenantId = (&r).try_into().unwrap();
        assert_eq!(t.as_str(), "acme");

        let bad = nexo_tool_meta::marketing::TenantIdRef("BAD".into());
        let err: Result<TenantId, _> = (&bad).try_into();
        assert!(err.is_err());
    }
}

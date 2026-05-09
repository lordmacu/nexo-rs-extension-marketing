//! Spam / promo filter — tenant-customizable.
//!
//! Sits between the broker decode hop (`broker::inbound`) and
//! the lead pipeline (`plugin::broker::handle_inbound_event`).
//! Default thresholds + keyword/role lists ship hardcoded; per
//! tenant the operator overlays their own block / allow rules
//! and picks a sensitivity preset (or fully custom thresholds)
//! through the marketing module's UI.
//!
//! Module layout:
//! - `defaults.rs`   — built-in keyword / role lists, preset thresholds
//! - `store.rs`      — SQLite schema + CRUD over rules + config
//! - `rules.rs`      — `ResolvedRules` (defaults ⊕ overrides) + cache
//! - `classifier.rs` — pure decision fn over signals + rules
//!
//! Verdict policy (high level):
//! 1. Allow rules first → bypass everything → `Human`
//! 2. Block rules → drop with reason
//! 3. Threshold-based signals (image-only, image-heavy,
//!    role+keyword, multi-weak) per the active preset
//!
//! Allow > block > threshold ordering means an operator can
//! always rescue a real sender that happens to match a noisy
//! default (e.g. an `info@` mailbox sending HTML brochures).

pub mod classifier;
pub mod defaults;
pub mod rules;
pub mod store;

pub use classifier::{classify_with_rules, BlockReason};
pub use defaults::{Strictness, ThresholdSet};
pub use rules::{ResolvedRules, RulesCache};
pub use store::{
    RuleKind, SpamFilterConfig, SpamFilterRule, SpamFilterStore, StoreError,
};

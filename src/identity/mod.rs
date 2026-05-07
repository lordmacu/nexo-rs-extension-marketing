//! Identity resolution pipeline.
//!
//! Inbound email → classify domain (corporate / personal /
//! disposable) → branch:
//!   - corporate: web scraper enrichment (separate
//!     `enrichment::scraper` module)
//!   - personal: walk the fallback chain (display name,
//!     signature, LLM extractor, cross-thread linker, Reply-To)
//!   - disposable: drop without notify
//!
//! Heavy lifting lives in `nexo-microapp-sdk::enrichment` (chain
//! runner, domain classifier, cache). This module contributes
//! the email-specific adapters + the resolver that drives them.

pub mod adapters;
pub mod resolver;

pub use resolver::{IdentityResolver, ResolveOutcome};

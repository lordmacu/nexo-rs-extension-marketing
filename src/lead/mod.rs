//! Lead state machine, sqlite store, routing dispatcher,
//! followup scheduler, scoring.
//!
//! Wire types come from `nexo_tool_meta::marketing` so the
//! extension + microapp + frontend agree on shapes.

pub mod state;

pub use state::{validate_transition, LegalTransitions};

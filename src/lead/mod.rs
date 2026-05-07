//! Lead state machine, sqlite store, routing dispatcher,
//! followup scheduler, scoring.
//!
//! Wire types come from `nexo_tool_meta::marketing` so the
//! extension + microapp + frontend agree on shapes.

pub mod router;
pub mod score;
pub mod state;
pub mod store;

pub use router::{load_rule_set, LeadRouter, RouteInputs, RouteOutcome};
pub use score::{score_lead, ScoreContribution, ScoreInputs, ScoreOutput};
pub use state::{validate_transition, LegalTransitions};
pub use store::{LeadStore, NewLead};

//! Broker subscribers + publishers.
//!
//! Inbound emails arrive on `plugin.inbound.email.<instance>`
//! per the email plugin's wire-spec; outbound commands go
//! back through `plugin.outbound.email.<instance>` so SMTP /
//! DKIM / anti-loop stay in the email plugin's hands.

pub mod inbound;

pub use inbound::{decode_inbound_email, ParsedInbound, ParseError, TenantResolver};

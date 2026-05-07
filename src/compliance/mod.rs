//! Outbound compliance middleware.
//!
//! Composes the framework's `nexo-compliance-primitives` into
//! a single `OutboundGate::check(...)` that the outbound
//! publisher calls before every SMTP enqueue. Decisions:
//!
//!   - `Send`            — body unchanged
//!   - `ModifyAndSend`   — PII redacted; ship the modified body
//!   - `Block(reason)`   — opt-out / anti-loop / rate-limit;
//!                         lead transitions to `Lost("compliance:
//!                         <reason>")` outside this module.

pub mod outbound_gate;

pub use outbound_gate::{Decision, OutboundGate};

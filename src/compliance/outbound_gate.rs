//! Pre-outbound compliance check.
//!
//! Every approved AI draft passes through `OutboundGate::check`
//! before publishing to `plugin.outbound.email.<instance>`.
//! The gate composes the framework's compliance primitives so
//! anti-loop / opt-out / PII redaction / rate-limit behave
//! consistently across every microapp that consumes them — we
//! never re-implement these heuristics, we wire them.

use std::sync::Mutex;
use std::time::Duration;

use nexo_compliance_primitives::anti_loop::{AntiLoopDetector, LoopVerdict};
use nexo_compliance_primitives::opt_out::{OptOutMatcher, OptOutVerdict};
use nexo_compliance_primitives::pii_redactor::PiiRedactor;
use nexo_compliance_primitives::rate_limit::{RateLimitPerUser, RateLimitVerdict};

/// What the gate tells the caller to do with the outbound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Body untouched — publish as-is.
    Send,
    /// Body was modified (PII redacted). Caller swaps the
    /// original body for `body` before publishing.
    ModifyAndSend {
        body: String,
        /// PII counters surfaced for telemetry / audit.
        redaction_summary: String,
    },
    /// Block + record `reason` on the lead's audit trail.
    /// Caller transitions the lead to `Lost("compliance: …")`.
    Block { reason: String },
}

/// Per-tenant gate. Holds mutable detector state behind a
/// `Mutex` so the same instance handles concurrent outbounds
/// safely.
pub struct OutboundGate {
    anti_loop: Mutex<AntiLoopDetector>,
    opt_out: OptOutMatcher,
    pii: PiiRedactor,
    rate_limit: Mutex<RateLimitPerUser>,
    /// `true` when PII findings should rewrite the body, `false`
    /// when they should block instead. Default redact (operator
    /// can flip per-tenant via YAML).
    redact_pii: bool,
}

impl OutboundGate {
    /// Defaults sized for B2B email volume:
    /// - anti-loop: same body 3+ times within 60 s = loop
    /// - opt-out: framework's curated phrase set ("unsubscribe",
    ///   "no quiero más correos", "STOP", …)
    /// - PII redactor: emails / phones / cards / Luhn
    /// - rate limit: 30 outbounds per recipient per hour
    pub fn with_defaults() -> Self {
        Self {
            anti_loop: Mutex::new(AntiLoopDetector::new(3, Duration::from_secs(60))),
            opt_out: OptOutMatcher::with_default_phrases(),
            pii: PiiRedactor::new().with_luhn(true),
            rate_limit: Mutex::new(RateLimitPerUser::flat(30, Duration::from_secs(3600))),
            redact_pii: true,
        }
    }

    /// Operator override: block on PII instead of redacting.
    /// Use when the empresa requires zero outbound PII (e.g.
    /// regulated industry).
    pub fn block_on_pii(mut self) -> Self {
        self.redact_pii = false;
        self
    }

    /// Run every check in order: anti-loop first (cheapest +
    /// most decisive), then opt-out, then PII (rewrite or
    /// block), then rate-limit.
    pub fn check(&self, recipient: &str, body: &str) -> Decision {
        // Anti-loop on the body — repeated identical bodies
        // within the window are almost always a runaway agent.
        match self.anti_loop.lock().unwrap().record_and_evaluate(body) {
            LoopVerdict::Clear => {}
            LoopVerdict::Repetition { count, .. } => {
                return Decision::Block {
                    reason: format!("anti_loop: same body {count} times in window"),
                };
            }
            LoopVerdict::AutoReplySignature { phrase } => {
                return Decision::Block {
                    reason: format!(
                        "anti_loop: outbound matches auto-reply signature {phrase:?}"
                    ),
                };
            }
        }

        // Opt-out matcher — for outbound we use it as a self-
        // spam tripwire (an agent that writes "unsubscribe" or
        // "STOP" into a draft is misbehaving). Operator wraps
        // legitimate "click to unsubscribe" in a footer the
        // matcher's curated phrase set ignores by default.
        match self.opt_out.evaluate(body) {
            OptOutVerdict::Clear => {}
            OptOutVerdict::OptOut { keyword } => {
                return Decision::Block {
                    reason: format!(
                        "opt_out: outbound body contains opt-out keyword {keyword:?}"
                    ),
                };
            }
        }

        // PII pass — operator chooses redact vs block.
        let (redacted, stats) = self.pii.redact(body);
        let body_changed = redacted != body;
        if body_changed {
            if !self.redact_pii {
                return Decision::Block {
                    reason: format!("pii: detected ({} findings)", stats.total()),
                };
            }
        }

        // Rate-limit per recipient.
        let mut rl = self.rate_limit.lock().unwrap();
        match rl.try_acquire(recipient) {
            RateLimitVerdict::Allowed { .. } => {}
            RateLimitVerdict::Denied { retry_after } => {
                return Decision::Block {
                    reason: format!(
                        "rate_limit: throttled — retry after {} ms",
                        retry_after.as_millis()
                    ),
                };
            }
        }

        if body_changed {
            return Decision::ModifyAndSend {
                body: redacted,
                redaction_summary: format!("{} pii fields redacted", stats.total()),
            };
        }
        Decision::Send
    }
}

impl Default for OutboundGate {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_body_sends_unchanged() {
        let g = OutboundGate::with_defaults();
        let d = g.check("juan@acme.com", "Hola Juan, te confirmo el martes.");
        assert_eq!(d, Decision::Send);
    }

    #[test]
    fn pii_in_body_redacts_when_default() {
        let g = OutboundGate::with_defaults();
        let d = g.check(
            "juan@acme.com",
            "Llamame al +57 311 555 5555 cuando puedas.",
        );
        match d {
            Decision::ModifyAndSend { body, .. } => {
                assert!(!body.contains("311 555 5555"));
            }
            other => panic!("expected ModifyAndSend, got {other:?}"),
        }
    }

    #[test]
    fn pii_blocks_when_block_on_pii_set() {
        let g = OutboundGate::with_defaults().block_on_pii();
        let d = g.check(
            "juan@acme.com",
            "Tarjeta de prueba: 4111 1111 1111 1111",
        );
        match d {
            Decision::Block { reason } => assert!(reason.starts_with("pii:")),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn anti_loop_blocks_repeated_body() {
        let g = OutboundGate::with_defaults();
        let body = "Same exact body";
        for _ in 0..2 {
            let d = g.check("a@x.com", body);
            assert!(matches!(d, Decision::Send));
        }
        let d = g.check("a@x.com", body);
        match d {
            Decision::Block { reason } => assert!(reason.starts_with("anti_loop:")),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn opt_out_phrase_in_outbound_blocks() {
        let g = OutboundGate::with_defaults();
        let d = g.check(
            "juan@acme.com",
            "Por favor click unsubscribe to stop receiving emails.",
        );
        match d {
            Decision::Block { reason } => assert!(reason.starts_with("opt_out:")),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_kicks_in_after_threshold() {
        let g = OutboundGate::with_defaults();
        // Default flat: 30 outbounds per recipient per hour.
        // First 30 distinct bodies pass; 31st blocks.
        for i in 0..30 {
            let d = g.check("burst@x.com", &format!("body {i}"));
            assert!(matches!(d, Decision::Send));
        }
        let d = g.check("burst@x.com", "body 31");
        match d {
            Decision::Block { reason } => assert!(reason.starts_with("rate_limit:")),
            other => panic!("expected Block, got {other:?}"),
        }
    }
}

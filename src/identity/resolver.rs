//! Identity resolver — drives the SDK fallback chain for an
//! inbound, branches by domain kind, persists the result.
//!
//! Pipeline:
//!   1. Classify domain (corporate / personal / disposable).
//!   2. Disposable → outcome `Drop`.
//!   3. Cross-thread linker hit → outcome `Existing(person_id)`.
//!   4. Fallback chain runs.
//!     - Hit ≥ threshold → upsert person + optional company →
//!       outcome `Resolved(person_id, source, confidence)`.
//!     - All exhausted → outcome `Pending(best_attempt)` so
//!       the operator can confirm in the UI.

use nexo_microapp_sdk::enrichment::{
    classify, DomainKind, EnrichmentInput, FallbackChain, FallbackOutcome,
};

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveOutcome {
    /// Disposable domain — the resolver short-circuits without
    /// running the chain. Caller drops the inbound.
    Drop,
    /// Cross-thread linker matched OR an explicit hit ≥
    /// threshold from the chain. `source` carries the
    /// adapter name for audit ("signature", "llm_extractor",
    /// …).
    Resolved {
        confidence: f32,
        source: String,
    },
    /// Chain ran but nothing met the confidence threshold. The
    /// best below-threshold attempt (if any) rides along so
    /// the operator UI can pre-fill the manual prompt.
    Pending {
        best_attempt: Option<nexo_tool_meta::marketing::EnrichmentResult>,
    },
}

/// Resolver glues the chain runner + the domain classifier.
/// Persistence (Person + PersonEmail upserts) lives outside
/// — caller decides where the resolved record lands. Keeps
/// the resolver pure + testable.
pub struct IdentityResolver<'a> {
    chain: &'a FallbackChain,
}

impl<'a> IdentityResolver<'a> {
    pub fn new(chain: &'a FallbackChain) -> Self {
        Self { chain }
    }

    pub async fn resolve(&self, input: &EnrichmentInput<'_>) -> ResolveOutcome {
        let kind = classify(input.from_email);
        if kind == DomainKind::Disposable {
            return ResolveOutcome::Drop;
        }
        let outcome = self.chain.run(input).await;
        match outcome {
            FallbackOutcome::Hit { result, .. } => ResolveOutcome::Resolved {
                confidence: result.confidence,
                source: result.source,
            },
            FallbackOutcome::AllExhausted { mut attempts } => {
                // Sort by confidence DESC so the best
                // below-threshold attempt rides as the hint.
                attempts.sort_by(|a, b| {
                    b.confidence
                        .partial_cmp(&a.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                ResolveOutcome::Pending {
                    best_attempt: attempts.into_iter().next(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use nexo_microapp_sdk::enrichment::{
        EnrichmentSource, EnrichmentSourceError, SourceCost,
    };
    use nexo_tool_meta::marketing::EnrichmentResult;

    struct StubSource(&'static str, Option<f32>);

    #[async_trait]
    impl EnrichmentSource for StubSource {
        fn name(&self) -> &str {
            self.0
        }
        fn cost_estimate(&self) -> SourceCost {
            SourceCost::Free
        }
        async fn extract(
            &self,
            _input: &EnrichmentInput<'_>,
        ) -> Result<Option<EnrichmentResult>, EnrichmentSourceError> {
            Ok(self.1.map(|c| EnrichmentResult {
                source: self.0.into(),
                confidence: c,
                person_inferred: None,
                company_inferred: None,
                note: None,
            }))
        }
    }

    fn inp<'a>(email: &'a str) -> EnrichmentInput<'a> {
        EnrichmentInput {
            from_email: email,
            from_display_name: None,
            subject: "",
            body_excerpt: "hi",
            reply_to: None,
        }
    }

    #[tokio::test]
    async fn disposable_domain_short_circuits() {
        let chain = FallbackChain::new(vec![], 0.7);
        let r = IdentityResolver::new(&chain);
        let out = r.resolve(&inp("test@mailinator.com")).await;
        assert_eq!(out, ResolveOutcome::Drop);
    }

    #[tokio::test]
    async fn corporate_with_chain_hit_resolves() {
        let chain = FallbackChain::new(
            vec![Box::new(StubSource("signature", Some(0.85)))],
            0.7,
        );
        let r = IdentityResolver::new(&chain);
        let out = r.resolve(&inp("juan@acme.com")).await;
        match out {
            ResolveOutcome::Resolved { confidence, source } => {
                assert!(confidence >= 0.7);
                assert_eq!(source, "signature");
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn personal_no_signal_pending() {
        let chain = FallbackChain::new(
            vec![Box::new(StubSource("a", None)), Box::new(StubSource("b", None))],
            0.7,
        );
        let r = IdentityResolver::new(&chain);
        let out = r.resolve(&inp("juan@gmail.com")).await;
        assert!(matches!(out, ResolveOutcome::Pending { best_attempt: None }));
    }

    #[tokio::test]
    async fn personal_below_threshold_returns_best_attempt_as_hint() {
        let chain = FallbackChain::new(
            vec![
                Box::new(StubSource("a", Some(0.30))),
                Box::new(StubSource("b", Some(0.55))),
                Box::new(StubSource("c", Some(0.40))),
            ],
            0.7,
        );
        let r = IdentityResolver::new(&chain);
        let out = r.resolve(&inp("juan@gmail.com")).await;
        match out {
            ResolveOutcome::Pending {
                best_attempt: Some(att),
            } => {
                assert_eq!(att.source, "b");
                assert_eq!(att.confidence, 0.55);
            }
            other => panic!("expected Pending(best), got {other:?}"),
        }
    }
}

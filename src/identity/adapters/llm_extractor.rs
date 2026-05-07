//! LLM-backed enrichment extractor. Sends `{from_email,
//! display_name, subject, body_excerpt}` to a small model
//! (haiku-class) with a strict JSON schema asking for
//! `company`, `role`, `intent`, `confidence`. Cached per
//! `(tenant_id, message_id)` upstream so re-runs don't double-
//! bill.
//!
//! The SDK doesn't ship an LLM client (provider-agnostic by
//! convention — every microapp wires its own). We define the
//! `LlmExtractorBackend` trait + a `NoopLlmExtractor` for
//! tests; the marketing extension's binary plugs in a real
//! backend (typically `ToolCtx::llm()` from the plugin SDK).

use async_trait::async_trait;

use nexo_microapp_sdk::enrichment::{
    EnrichmentInput, EnrichmentSource, EnrichmentSourceError, SourceCost,
};
use nexo_tool_meta::marketing::{CompanyInferred, EnrichmentResult, PersonInferred};

/// Pluggable LLM call. Implementations live in the binary;
/// tests use `NoopLlmExtractor` or a stub.
#[async_trait]
pub trait LlmExtractorBackend: Send + Sync {
    /// Run the LLM. Returns `Ok(None)` for "model said it
    /// can't infer anything", `Ok(Some(_))` for a structured
    /// answer, `Err(_)` for transport / quota / timeout.
    async fn classify(
        &self,
        input: &EnrichmentInput<'_>,
    ) -> Result<Option<LlmExtractResponse>, EnrichmentSourceError>;
}

/// JSON shape the backend returns. Confidence stays 0.0..=1.0;
/// the chain runner enforces the threshold.
#[derive(Debug, Clone)]
pub struct LlmExtractResponse {
    pub company_name: Option<String>,
    pub company_domain: Option<String>,
    pub company_industry: Option<String>,
    pub person_role: Option<String>,
    pub confidence: f32,
    pub note: Option<String>,
}

/// Generic adapter that wraps any backend. The marketing
/// extension instantiates it with its real LLM client.
pub struct LlmExtractor<B: LlmExtractorBackend> {
    backend: B,
}

impl<B: LlmExtractorBackend> LlmExtractor<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: LlmExtractorBackend> EnrichmentSource for LlmExtractor<B> {
    fn name(&self) -> &str {
        "llm_extractor"
    }
    fn cost_estimate(&self) -> SourceCost {
        SourceCost::Moderate
    }
    async fn extract(
        &self,
        input: &EnrichmentInput<'_>,
    ) -> Result<Option<EnrichmentResult>, EnrichmentSourceError> {
        if input.body_excerpt.is_empty() {
            return Ok(None);
        }
        let resp = match self.backend.classify(input).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        Ok(Some(EnrichmentResult {
            source: "llm_extractor".into(),
            confidence: resp.confidence.clamp(0.0, 1.0),
            person_inferred: resp.person_role.map(|role| PersonInferred {
                name: None,
                role: Some(role),
                seniority: None,
            }),
            company_inferred: if resp.company_name.is_some() || resp.company_domain.is_some() {
                Some(CompanyInferred {
                    name: resp.company_name,
                    domain: resp.company_domain,
                    industry: resp.company_industry,
                })
            } else {
                None
            },
            note: resp.note,
        }))
    }
}

/// Always-`None` backend — useful in tests / when the operator
/// hasn't configured an LLM provider yet.
pub struct NoopLlmExtractor;

#[async_trait]
impl LlmExtractorBackend for NoopLlmExtractor {
    async fn classify(
        &self,
        _input: &EnrichmentInput<'_>,
    ) -> Result<Option<LlmExtractResponse>, EnrichmentSourceError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inp<'a>(body: &'a str) -> EnrichmentInput<'a> {
        EnrichmentInput {
            from_email: "juan@gmail.com",
            from_display_name: None,
            subject: "Hi",
            body_excerpt: body,
            reply_to: None,
        }
    }

    struct StubBackend(Option<LlmExtractResponse>);

    #[async_trait]
    impl LlmExtractorBackend for StubBackend {
        async fn classify(
            &self,
            _i: &EnrichmentInput<'_>,
        ) -> Result<Option<LlmExtractResponse>, EnrichmentSourceError> {
            Ok(self.0.clone().map(|r| LlmExtractResponse {
                company_name: r.company_name,
                company_domain: r.company_domain,
                company_industry: r.company_industry,
                person_role: r.person_role,
                confidence: r.confidence,
                note: r.note,
            }))
        }
    }

    // (Clone derived on the type already; no manual impl needed.)

    #[tokio::test]
    async fn returns_none_on_empty_body() {
        let e = LlmExtractor::new(NoopLlmExtractor);
        let r = e.extract(&inp("")).await.unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn noop_backend_returns_none() {
        let e = LlmExtractor::new(NoopLlmExtractor);
        let r = e.extract(&inp("hola")).await.unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn stub_backend_with_company_returns_result() {
        let e = LlmExtractor::new(StubBackend(Some(LlmExtractResponse {
            company_name: Some("Acme".into()),
            company_domain: Some("acme.com".into()),
            company_industry: Some("fintech".into()),
            person_role: Some("VP Sales".into()),
            confidence: 0.78,
            note: Some("inferred from body".into()),
        })));
        let r = e.extract(&inp("hola")).await.unwrap().unwrap();
        assert_eq!(r.confidence, 0.78);
        assert_eq!(r.company_inferred.unwrap().name.unwrap(), "Acme");
        assert_eq!(r.person_inferred.unwrap().role.unwrap(), "VP Sales");
    }

    #[tokio::test]
    async fn confidence_clamped_to_unit_range() {
        let e = LlmExtractor::new(StubBackend(Some(LlmExtractResponse {
            company_name: Some("Acme".into()),
            company_domain: None,
            company_industry: None,
            person_role: None,
            confidence: 1.5,
            note: None,
        })));
        let r = e.extract(&inp("hola")).await.unwrap().unwrap();
        assert!((0.0..=1.0).contains(&r.confidence));
    }

    #[tokio::test]
    async fn no_company_no_role_yields_none_company_inferred() {
        let e = LlmExtractor::new(StubBackend(Some(LlmExtractResponse {
            company_name: None,
            company_domain: None,
            company_industry: None,
            person_role: None,
            confidence: 0.5,
            note: None,
        })));
        let r = e.extract(&inp("hola")).await.unwrap().unwrap();
        assert!(r.company_inferred.is_none());
        assert!(r.person_inferred.is_none());
    }
}

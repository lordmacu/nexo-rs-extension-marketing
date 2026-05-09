//! Reply-To header reader. When the visible `From:` is a
//! personal address but the message carries a `Reply-To:` (or
//! `In-Reply-To:` originating address) at a corporate domain,
//! that's almost always the operator using a personal forward
//! that ultimately lands in the corporate mailbox. Free; deterministic.
//!
//! **F29 sweep:** marketing-specific impl of the SDK's
//! `EnrichmentSource`. Header parsing logic could lift if a
//! second consumer needs Reply-To extraction outside the CRM
//! envelope shape; today the chain runner from the SDK is the
//! generic seam.

use async_trait::async_trait;

use nexo_microapp_sdk::enrichment::{
    classify, DomainKind, EnrichmentInput, EnrichmentSource, EnrichmentSourceError,
    SourceCost,
};
use nexo_tool_meta::marketing::{CompanyInferred, EnrichmentResult};

pub struct ReplyToReader;

#[async_trait]
impl EnrichmentSource for ReplyToReader {
    fn name(&self) -> &str {
        "reply_to"
    }
    fn cost_estimate(&self) -> SourceCost {
        SourceCost::Free
    }
    async fn extract(
        &self,
        input: &EnrichmentInput<'_>,
    ) -> Result<Option<EnrichmentResult>, EnrichmentSourceError> {
        let reply_to = match input.reply_to {
            Some(r) if !r.trim().is_empty() => r.trim(),
            _ => return Ok(None),
        };
        // Useful only when From is personal AND Reply-To is
        // corporate — otherwise From itself is the canonical.
        let from_kind = classify(input.from_email);
        let reply_kind = classify(reply_to);
        if from_kind != DomainKind::Personal || reply_kind != DomainKind::Corporate {
            return Ok(None);
        }
        let domain = match reply_to.rsplit_once('@') {
            Some((_, d)) => d.to_string(),
            None => return Ok(None),
        };
        Ok(Some(EnrichmentResult {
            source: "reply_to".into(),
            confidence: 0.85,
            person_inferred: None,
            company_inferred: Some(CompanyInferred {
                name: None,
                domain: Some(domain.clone()),
                industry: None,
            }),
            note: Some(format!(
                "Reply-To header redirects to corporate domain {domain}"
            )),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inp<'a>(from: &'a str, reply_to: Option<&'a str>) -> EnrichmentInput<'a> {
        EnrichmentInput {
            from_email: from,
            from_display_name: None,
            subject: "",
            body_excerpt: "",
            reply_to,
        }
    }

    #[tokio::test]
    async fn personal_from_with_corporate_reply_to_hits() {
        let r = ReplyToReader
            .extract(&inp("juan@gmail.com", Some("juan@acme.com")))
            .await
            .unwrap()
            .unwrap();
        assert!(r.confidence >= 0.8);
        assert_eq!(
            r.company_inferred.unwrap().domain.unwrap(),
            "acme.com"
        );
    }

    #[tokio::test]
    async fn corporate_from_does_not_use_reply_to() {
        // From is already corporate — Reply-To redundant.
        let r = ReplyToReader
            .extract(&inp("juan@acme.com", Some("juan@globex.io")))
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn personal_to_personal_reply_to_misses() {
        // Both gmail — no signal.
        let r = ReplyToReader
            .extract(&inp("juan@gmail.com", Some("ana@outlook.com")))
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn empty_reply_to_misses() {
        let r = ReplyToReader
            .extract(&inp("juan@gmail.com", None))
            .await
            .unwrap();
        assert!(r.is_none());
        let r2 = ReplyToReader
            .extract(&inp("juan@gmail.com", Some("   ")))
            .await
            .unwrap();
        assert!(r2.is_none());
    }
}

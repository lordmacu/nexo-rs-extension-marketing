//! Email signature parser. Reads the last 4-6 non-quoted
//! lines of the body, regex-extracts `name | role | company`
//! shapes common in B2B signatures.
//!
//! Cheap (regex over a few hundred bytes, no IO). Confidence
//! 0.7-0.95 depending on signal density.

use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;

use nexo_microapp_sdk::enrichment::{
    EnrichmentInput, EnrichmentSource, EnrichmentSourceError, SourceCost,
};
use nexo_tool_meta::marketing::{CompanyInferred, EnrichmentResult, PersonInferred};

/// Lines like:
///   "Pedro García | VP Sales | Acme Corp"
///   "Pedro García - Account Manager - Acme"
///   "Pedro García, Marketing Director, MiEmpresa"
static SIG_TRIPLE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*([A-ZÁÉÍÓÚÑ][\w'.\-]+(?:\s+[A-ZÁÉÍÓÚÑ][\w'.\-]+){0,3})\s*[|\-,]\s*([\w\s/&]{2,60}?)\s*[|\-,]\s*([A-Z][\w\s'.&\-]{1,60})\s*$").unwrap()
});

/// Lines like "VP Sales at Acme Corp" / "Marketing @ Acme".
static ROLE_AT_COMPANY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^\s*([\w\s/&]{2,60})\s+(?:at|@)\s+([A-Z][\w\s'.&\-]{1,60})\s*$").unwrap()
});

/// Standalone "Acme Corp" / "Acme Inc." / "MiEmpresa S.A.S."
/// line — usually appears below the role.
static COMPANY_LINE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*([A-Z][\w\s'.&\-]{1,60}\s+(?:Corp|Inc|Ltd|LLC|S\.A|S\.A\.S|S\.L|GmbH|AG|Co\.|Company|Solutions|Group|Holdings|Partners))\s*$").unwrap()
});

const SIG_TAIL_LINES: usize = 8;

pub struct SignatureParser;

#[async_trait]
impl EnrichmentSource for SignatureParser {
    fn name(&self) -> &str {
        "signature"
    }
    fn cost_estimate(&self) -> SourceCost {
        SourceCost::Free
    }
    async fn extract(
        &self,
        input: &EnrichmentInput<'_>,
    ) -> Result<Option<EnrichmentResult>, EnrichmentSourceError> {
        if input.body_excerpt.is_empty() {
            return Ok(None);
        }
        // Strip quoted reply lines (`> ` prefix) and grab the
        // last N non-empty lines — that's where signatures
        // live. Mail clients almost always put the signature
        // directly above the quoted history.
        let candidate: Vec<&str> = input
            .body_excerpt
            .lines()
            .rev()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('>'))
            .take(SIG_TAIL_LINES)
            .collect();
        let block: String = candidate.iter().rev().copied().collect::<Vec<_>>().join("\n");

        // Triple shape — strongest signal.
        if let Some(caps) = SIG_TRIPLE.captures(&block) {
            let name = caps[1].trim().to_string();
            let role = caps[2].trim().to_string();
            let company = caps[3].trim().to_string();
            return Ok(Some(EnrichmentResult {
                source: "signature".into(),
                confidence: 0.92,
                person_inferred: Some(PersonInferred {
                    name: Some(name),
                    role: Some(role.clone()),
                    seniority: infer_seniority(&role),
                }),
                company_inferred: Some(CompanyInferred {
                    name: Some(company),
                    domain: None,
                    industry: None,
                }),
                note: Some("matched name|role|company triple".into()),
            }));
        }
        // Role @ company — moderate signal.
        if let Some(caps) = ROLE_AT_COMPANY.captures(&block) {
            let role = caps[1].trim().to_string();
            let company = caps[2].trim().to_string();
            return Ok(Some(EnrichmentResult {
                source: "signature".into(),
                confidence: 0.80,
                person_inferred: Some(PersonInferred {
                    name: None,
                    role: Some(role.clone()),
                    seniority: infer_seniority(&role),
                }),
                company_inferred: Some(CompanyInferred {
                    name: Some(company),
                    domain: None,
                    industry: None,
                }),
                note: Some("matched 'role at company' shape".into()),
            }));
        }
        // Standalone "Acme Corp" line.
        if let Some(caps) = COMPANY_LINE.captures(&block) {
            let company = caps[1].trim().to_string();
            return Ok(Some(EnrichmentResult {
                source: "signature".into(),
                confidence: 0.70,
                person_inferred: None,
                company_inferred: Some(CompanyInferred {
                    name: Some(company),
                    domain: None,
                    industry: None,
                }),
                note: Some("matched 'CompanyName Suffix' line".into()),
            }));
        }
        Ok(None)
    }
}

fn infer_seniority(role: &str) -> Option<String> {
    // Tokenise on whitespace + punctuation so "director"
    // doesn't substring-match "cto" inside "dire(cto)r".
    let r = role.to_lowercase();
    let tokens: Vec<&str> = r
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let has_token = |needle: &str| tokens.iter().any(|t| *t == needle);

    if has_token("ceo") || has_token("founder") || r.contains("chief executive") {
        return Some("C-level".into());
    }
    if has_token("cto") || has_token("cmo") || has_token("cfo") || has_token("coo") {
        return Some("C-level".into());
    }
    if has_token("vp") || r.contains("vice president") {
        return Some("VP".into());
    }
    if has_token("director") {
        return Some("Director".into());
    }
    if has_token("manager") || has_token("lead") || r.contains("head of") {
        return Some("Manager".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_with_body<'a>(body: &'a str) -> EnrichmentInput<'a> {
        EnrichmentInput {
            from_email: "juan@gmail.com",
            from_display_name: None,
            subject: "",
            body_excerpt: body,
            reply_to: None,
        }
    }

    #[tokio::test]
    async fn matches_pipe_separated_triple() {
        let body = "Hola Pedro, te confirmo la reunión.\n\nSaludos,\nJuan García | VP Sales | Acme Corp";
        let r = SignatureParser
            .extract(&input_with_body(body))
            .await
            .unwrap()
            .unwrap();
        assert!(r.confidence >= 0.9);
        let p = r.person_inferred.unwrap();
        assert_eq!(p.name.unwrap(), "Juan García");
        assert_eq!(p.role.unwrap(), "VP Sales");
        assert_eq!(p.seniority, Some("VP".into()));
        let c = r.company_inferred.unwrap();
        assert_eq!(c.name.unwrap(), "Acme Corp");
    }

    #[tokio::test]
    async fn matches_dash_separated_triple() {
        let body = "\n\nMaría López - Marketing Director - Acme";
        let r = SignatureParser
            .extract(&input_with_body(body))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            r.person_inferred.unwrap().seniority,
            Some("Director".into())
        );
    }

    #[tokio::test]
    async fn matches_role_at_company() {
        let body = "Cheers,\n\nVP Sales at Acme Corp";
        let r = SignatureParser
            .extract(&input_with_body(body))
            .await
            .unwrap()
            .unwrap();
        assert!(r.confidence >= 0.7 && r.confidence < 0.9);
        let p = r.person_inferred.unwrap();
        assert!(p.name.is_none());
        assert_eq!(p.role.unwrap(), "VP Sales");
    }

    #[tokio::test]
    async fn matches_standalone_company_line() {
        let body = "Best,\n\nAcme Corp";
        let r = SignatureParser
            .extract(&input_with_body(body))
            .await
            .unwrap()
            .unwrap();
        assert!(r.confidence < 0.8);
        assert!(r.person_inferred.is_none());
        assert_eq!(r.company_inferred.unwrap().name.unwrap(), "Acme Corp");
    }

    #[tokio::test]
    async fn ignores_quoted_reply_lines() {
        // The triple lives ONLY inside the quoted reply →
        // signature parser MUST NOT match it.
        let body = "Hola, gracias.\n\n> Juan García | VP Sales | Acme Corp";
        let r = SignatureParser.extract(&input_with_body(body)).await.unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn empty_body_returns_none() {
        let r = SignatureParser.extract(&input_with_body("")).await.unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn ceo_role_marks_c_level() {
        let body = "\n\nMaría - CEO - Acme";
        let r = SignatureParser
            .extract(&input_with_body(body))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            r.person_inferred.unwrap().seniority,
            Some("C-level".into())
        );
    }
}

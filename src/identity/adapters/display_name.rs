//! Display-name parser. Extracts company name from the `From:`
//! display string when the operator's signature carries
//! `"Juan García (Acme Corp)" <juan@gmail.com>` shape — common
//! in Latam B2B where corporate users send from personal
//! providers.
//!
//! Cheap (regex, no IO). Confidence band 0.6-0.85 depending
//! on signal strength.

use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;

use nexo_microapp_sdk::enrichment::{
    EnrichmentInput, EnrichmentSource, EnrichmentSourceError, SourceCost,
};
use nexo_tool_meta::marketing::{CompanyInferred, EnrichmentResult, PersonInferred};

/// Matches `Name (Company)` where the parens contain a non-
/// trivial chunk. Permissive — falls back to `None` quietly.
static PAREN_COMPANY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?P<name>[^()]+?)\s*\(\s*(?P<company>[^)]{2,80})\s*\)\s*$").unwrap());

pub struct DisplayNameParser;

#[async_trait]
impl EnrichmentSource for DisplayNameParser {
    fn name(&self) -> &str {
        "display_name"
    }
    fn cost_estimate(&self) -> SourceCost {
        SourceCost::Free
    }
    async fn extract(
        &self,
        input: &EnrichmentInput<'_>,
    ) -> Result<Option<EnrichmentResult>, EnrichmentSourceError> {
        let display = match input.from_display_name {
            Some(d) if !d.trim().is_empty() => d.trim(),
            _ => return Ok(None),
        };
        if let Some(caps) = PAREN_COMPANY.captures(display) {
            let name = caps["name"].trim().to_string();
            let company = caps["company"].trim().to_string();
            // Reject obviously non-company tokens (single
            // common words, all-lowercase short, etc.).
            if !looks_like_company(&company) {
                return Ok(None);
            }
            return Ok(Some(EnrichmentResult {
                source: "display_name".into(),
                confidence: 0.75,
                person_inferred: Some(PersonInferred {
                    name: Some(name),
                    role: None,
                    seniority: None,
                }),
                company_inferred: Some(CompanyInferred {
                    name: Some(company),
                    domain: None,
                    industry: None,
                }),
                note: Some("matched display-name parens shape".into()),
            }));
        }
        // Display name without parens — extract person name only.
        if !display.contains('@') && display.len() <= 80 {
            return Ok(Some(EnrichmentResult {
                source: "display_name".into(),
                confidence: 0.45,
                person_inferred: Some(PersonInferred {
                    name: Some(display.to_string()),
                    role: None,
                    seniority: None,
                }),
                company_inferred: None,
                note: Some("display name only, no parens".into()),
            }));
        }
        Ok(None)
    }
}

fn looks_like_company(s: &str) -> bool {
    let t = s.trim();
    if t.len() < 2 || t.len() > 80 {
        return false;
    }
    // Reject pure lowercase short words (likely a tag, not a
    // company): "ok", "hi", "team".
    if t.len() <= 4 && t.chars().all(|c| c.is_ascii_lowercase()) {
        return false;
    }
    // Looks like a person name? "Juan Carlos" — single space,
    // both capitalised words. Reject.
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts.len() == 2
        && parts.iter().all(|p| {
            p.chars().next().map_or(false, |c| c.is_ascii_uppercase())
                && !p.contains(['.', ',', '&', 'S'].iter().copied().collect::<Vec<_>>().as_slice())
        })
    {
        // Heuristic: probably a "Last, First" or "First Middle"
        // person name. Uncertain; let the signature parser
        // catch the company instead.
        let has_corp_marker = t.contains(['.', ',', '&'])
            || t.to_lowercase().contains(" corp")
            || t.to_lowercase().contains(" inc")
            || t.to_lowercase().contains(" ltd")
            || t.to_lowercase().contains(" llc")
            || t.to_lowercase().contains(" sa")
            || t.to_lowercase().contains(" sas")
            || t.to_lowercase().contains(" sl");
        if !has_corp_marker {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(display: Option<&'a str>) -> EnrichmentInput<'a> {
        EnrichmentInput {
            from_email: "juan@gmail.com",
            from_display_name: display,
            subject: "",
            body_excerpt: "",
            reply_to: None,
        }
    }

    #[tokio::test]
    async fn extracts_company_from_parens() {
        let r = DisplayNameParser
            .extract(&input(Some("Juan García (Acme Corp)")))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.source, "display_name");
        assert!(r.confidence >= 0.7);
        assert_eq!(
            r.company_inferred.unwrap().name.unwrap(),
            "Acme Corp".to_string()
        );
        assert_eq!(
            r.person_inferred.unwrap().name.unwrap(),
            "Juan García".to_string()
        );
    }

    #[tokio::test]
    async fn rejects_two_word_person_name_in_parens() {
        // "Juan Carlos" — likely middle name, not a company.
        let r = DisplayNameParser
            .extract(&input(Some("Juan García (Juan Carlos)")))
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn empty_display_returns_none() {
        let r = DisplayNameParser.extract(&input(None)).await.unwrap();
        assert!(r.is_none());
        let r2 = DisplayNameParser
            .extract(&input(Some("   ")))
            .await
            .unwrap();
        assert!(r2.is_none());
    }

    #[tokio::test]
    async fn name_only_without_parens_low_confidence() {
        let r = DisplayNameParser
            .extract(&input(Some("María López")))
            .await
            .unwrap()
            .unwrap();
        assert!(r.confidence < 0.5);
        assert!(r.company_inferred.is_none());
        assert!(r.person_inferred.is_some());
    }

    #[tokio::test]
    async fn corporate_marker_two_word_accepted() {
        let r = DisplayNameParser
            .extract(&input(Some("Juan (Acme Inc)")))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.company_inferred.unwrap().name.unwrap(), "Acme Inc");
    }
}

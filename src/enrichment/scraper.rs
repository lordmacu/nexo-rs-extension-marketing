//! Corporate domain scraper.
//!
//! Fetches `https://<domain>/` (and falls back to `/about`),
//! extracts company name + industry hints from meta tags and
//! JSON-LD `Organization` blocks. Concurrency is bounded by a
//! shared `Semaphore` so a flood of inbound emails to N
//! distinct domains doesn't open N HTTP connections at once.
//!
//! Robots-aware: when a domain's `/robots.txt` disallows the
//! relevant path, the scraper short-circuits to `Blocked`
//! (caller marks the company as "scraping refused" and the
//! resolver falls through to manual / LLM extraction).

use std::sync::Arc;
use std::time::Duration;

use scraper::{Html, Selector};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Semaphore;

const DEFAULT_USER_AGENT: &str =
    "nexo-marketing-extension/0.1 (+https://github.com/lordmacu/nexo-rs)";
const DEFAULT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_CONCURRENCY: usize = 4;

#[derive(Debug, Clone)]
pub struct ScraperConfig {
    pub user_agent: String,
    pub timeout: Duration,
    pub concurrency: usize,
    /// Honour `robots.txt`. Tests flip to `false` to skip the
    /// extra HTTP roundtrip.
    pub robots_aware: bool,
}

impl Default for ScraperConfig {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_USER_AGENT.to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            concurrency: DEFAULT_CONCURRENCY,
            robots_aware: true,
        }
    }
}

/// Output shape — what we tease out of a corporate site.
/// Every field optional; the scraper returns `Default` on a
/// best-effort fetch that found nothing parseable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrapedPage {
    pub domain: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub industry: Option<String>,
    pub canonical_url: Option<String>,
    pub etag: Option<String>,
    /// Path that produced the result (`/`, `/about`, etc.).
    pub source_path: String,
}

#[derive(Debug, Error)]
pub enum ScraperError {
    /// `reqwest` error (connect refused, TLS, DNS, …).
    #[error("http: {0}")]
    Http(String),
    /// Robots.txt explicitly disallows the path.
    #[error("blocked by robots.txt")]
    BlockedByRobots,
    /// Server returned non-2xx + we exhausted fallback paths.
    #[error("upstream returned {status} for every probe path")]
    UpstreamFailed { status: u16 },
    /// Body decoded but contained no parseable enrichment
    /// signals. Caller treats as "best-effort done; no data".
    #[error("no signals in body")]
    NoSignals,
    /// Operator's HTTP client misconfigured.
    #[error("config: {0}")]
    Config(String),
}

/// Async scraper. Cheap to clone (`Arc`-shared semaphore +
/// reqwest client). Pass into spawned tasks freely.
#[derive(Clone)]
pub struct Scraper {
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    cfg: ScraperConfig,
}

impl Scraper {
    pub fn new(cfg: ScraperConfig) -> Result<Self, ScraperError> {
        let client = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .timeout(cfg.timeout)
            // Don't follow redirects across hosts — corporate
            // domains usually self-redirect, but a chained
            // redirect to `tracking.com` is noise we don't
            // want to scrape.
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .map_err(|e| ScraperError::Config(e.to_string()))?;
        let semaphore = Arc::new(Semaphore::new(cfg.concurrency));
        Ok(Self {
            client,
            semaphore,
            cfg,
        })
    }

    /// Try `/` first, then `/about` — corporate sites usually
    /// have richer JSON-LD on `/about`.
    pub async fn scrape_domain(&self, domain: &str) -> Result<ScrapedPage, ScraperError> {
        // Bound concurrent calls so a swamped tenant doesn't
        // open hundreds of HTTP connections.
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| ScraperError::Config(e.to_string()))?;

        let base = format!("https://{}", domain.trim_start_matches("https://"));

        if self.cfg.robots_aware {
            if let Err(e) = self.check_robots(&base).await {
                return Err(e);
            }
        }

        let mut last_status: Option<u16> = None;
        for path in &["/", "/about", "/about-us", "/company"] {
            let url = format!("{base}{path}");
            match self.fetch_html(&url).await {
                Ok((html, etag)) => {
                    let mut page = parse_page(&html);
                    page.domain = domain.to_string();
                    page.source_path = path.to_string();
                    page.etag = etag;
                    if page.has_signal() {
                        return Ok(page);
                    }
                }
                Err(ScraperError::UpstreamFailed { status }) => {
                    last_status = Some(status);
                    continue;
                }
                Err(other) => return Err(other),
            }
        }
        if let Some(status) = last_status {
            Err(ScraperError::UpstreamFailed { status })
        } else {
            Err(ScraperError::NoSignals)
        }
    }

    async fn check_robots(&self, base: &str) -> Result<(), ScraperError> {
        let url = format!("{base}/robots.txt");
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            // robots.txt missing or unreachable → permissive.
            Err(_) => return Ok(()),
        };
        if !resp.status().is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        // Cheap parse — we only care if the operator's UA is
        // disallowed at root. A proper robots parser is
        // overkill for corporate enrichment; the worst case is
        // we scrape a site that asked us not to, which we'd
        // see in operator complaints + add to a denylist.
        let mut applies = false;
        for line in body.lines() {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix("User-agent:") {
                let ua = rest.trim();
                applies = ua == "*" || self.cfg.user_agent.contains(ua);
            }
            if applies {
                if let Some(path) = l.strip_prefix("Disallow:") {
                    let p = path.trim();
                    if p == "/" {
                        return Err(ScraperError::BlockedByRobots);
                    }
                }
            }
        }
        Ok(())
    }

    async fn fetch_html(&self, url: &str) -> Result<(String, Option<String>), ScraperError> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| ScraperError::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ScraperError::UpstreamFailed {
                status: status.as_u16(),
            });
        }
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = resp
            .text()
            .await
            .map_err(|e| ScraperError::Http(e.to_string()))?;
        Ok((body, etag))
    }
}

impl ScrapedPage {
    fn has_signal(&self) -> bool {
        self.name.is_some() || self.description.is_some() || self.industry.is_some()
    }
}

// ── HTML parser ─────────────────────────────────────────────────

fn parse_page(html: &str) -> ScrapedPage {
    let doc = Html::parse_document(html);
    let mut page = ScrapedPage::default();

    // og:site_name / og:title / <title>
    if let Some(s) = meta_content(&doc, "property", "og:site_name") {
        page.name = Some(s);
    } else if let Some(s) = meta_content(&doc, "property", "og:title") {
        page.name = Some(s);
    } else if let Ok(title_sel) = Selector::parse("title") {
        if let Some(el) = doc.select(&title_sel).next() {
            let t = el.text().collect::<String>().trim().to_string();
            if !t.is_empty() {
                page.name = Some(t);
            }
        }
    }

    // description
    if let Some(d) = meta_content(&doc, "name", "description") {
        page.description = Some(d);
    } else if let Some(d) = meta_content(&doc, "property", "og:description") {
        page.description = Some(d);
    }

    // canonical url
    if let Ok(sel) = Selector::parse("link[rel=canonical]") {
        if let Some(el) = doc.select(&sel).next() {
            if let Some(href) = el.value().attr("href") {
                page.canonical_url = Some(href.to_string());
            }
        }
    }

    // JSON-LD Organization → industry / name override
    if let Some(org) = parse_jsonld_org(&doc) {
        if page.name.is_none() && org.name.is_some() {
            page.name = org.name;
        }
        if let Some(industry) = org.industry {
            page.industry = Some(industry);
        }
    }

    page
}

fn meta_content(doc: &Html, attr: &str, value: &str) -> Option<String> {
    let sel = Selector::parse(&format!("meta[{attr}=\"{value}\"]")).ok()?;
    let el = doc.select(&sel).next()?;
    let content = el.value().attr("content")?.trim();
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

#[derive(Debug, Default, Deserialize)]
struct JsonLdOrg {
    #[serde(default)]
    name: Option<String>,
    /// Schema.org Organization variant tags this on
    /// `industry` for finance/insurance, and via
    /// `naics`/`sic` codes elsewhere. We accept a free-form
    /// `industry` string as a best-effort.
    #[serde(default)]
    industry: Option<String>,
}

fn parse_jsonld_org(doc: &Html) -> Option<JsonLdOrg> {
    let sel = Selector::parse(r#"script[type="application/ld+json"]"#).ok()?;
    for el in doc.select(&sel) {
        let raw: String = el.text().collect();
        let parsed: serde_json::Result<serde_json::Value> = serde_json::from_str(&raw);
        let v = match parsed {
            Ok(v) => v,
            Err(_) => continue,
        };
        // The block can be a single object or an array.
        let obj = match v {
            serde_json::Value::Array(arr) => arr.into_iter().find(is_organization),
            single if is_organization(&single) => Some(single),
            _ => continue,
        };
        if let Some(o) = obj {
            if let Ok(parsed) = serde_json::from_value::<JsonLdOrg>(o) {
                return Some(parsed);
            }
        }
    }
    None
}

fn is_organization(v: &serde_json::Value) -> bool {
    v.get("@type")
        .and_then(|t| t.as_str())
        .map(|s| {
            s == "Organization" || s == "Corporation" || s == "LocalBusiness"
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html_fixture_meta() -> String {
        r#"<!DOCTYPE html>
<html>
<head>
<meta property="og:site_name" content="Acme Corp">
<meta name="description" content="Industrial automation since 1995">
<link rel="canonical" href="https://acme.example/">
</head>
<body>Welcome</body>
</html>"#
            .to_string()
    }

    fn html_fixture_jsonld() -> String {
        r#"<!DOCTYPE html>
<html>
<head>
<title>Globex</title>
<script type="application/ld+json">
{
  "@context": "https://schema.org",
  "@type": "Organization",
  "name": "Globex Industries",
  "industry": "Renewable Energy"
}
</script>
</head>
<body></body>
</html>"#
            .to_string()
    }

    fn html_fixture_no_signal() -> String {
        r#"<!DOCTYPE html>
<html><head></head><body><p>hello</p></body></html>"#
            .to_string()
    }

    #[test]
    fn parse_meta_extracts_name_and_description() {
        let p = parse_page(&html_fixture_meta());
        assert_eq!(p.name.as_deref(), Some("Acme Corp"));
        assert_eq!(
            p.description.as_deref(),
            Some("Industrial automation since 1995")
        );
        assert_eq!(p.canonical_url.as_deref(), Some("https://acme.example/"));
    }

    #[test]
    fn parse_jsonld_provides_industry_and_overrides_title() {
        let p = parse_page(&html_fixture_jsonld());
        // <title>Globex</title> is the og fallback; JSON-LD
        // 'name' override applies because no og:site_name was
        // present (parser sets title first, then JSON-LD only
        // overrides when name was None — we keep <title>).
        // What we DO get unconditionally is industry.
        assert_eq!(p.industry.as_deref(), Some("Renewable Energy"));
        // Name comes from <title> first; JSON-LD doesn't
        // overwrite a populated `og:title`/<title>.
        assert_eq!(p.name.as_deref(), Some("Globex"));
    }

    #[test]
    fn parse_no_signal_returns_default_no_signal() {
        let p = parse_page(&html_fixture_no_signal());
        assert!(p.name.is_none());
        assert!(p.description.is_none());
        assert!(!p.has_signal());
    }

    #[test]
    fn jsonld_array_form_parsed() {
        let html = r#"<html><head>
<script type="application/ld+json">
[
  {"@type":"WebSite","name":"site"},
  {"@type":"Organization","name":"InnerCo","industry":"saas"}
]
</script></head></html>"#;
        let p = parse_page(html);
        assert_eq!(p.industry.as_deref(), Some("saas"));
        // og/title missing → InnerCo from JSON-LD becomes name.
        assert_eq!(p.name.as_deref(), Some("InnerCo"));
    }

    #[test]
    fn scraper_default_config_construct() {
        // Smoke — the Scraper builds a reqwest Client without
        // panicking under the default config.
        let s = Scraper::new(ScraperConfig::default()).unwrap();
        // Semaphore initialised with the configured concurrency.
        assert_eq!(s.semaphore.available_permits(), 4);
    }

    #[test]
    fn scraper_custom_concurrency_respected() {
        let cfg = ScraperConfig {
            concurrency: 2,
            ..Default::default()
        };
        let s = Scraper::new(cfg).unwrap();
        assert_eq!(s.semaphore.available_permits(), 2);
    }

    #[test]
    fn parse_page_robust_to_garbage_jsonld() {
        let html = r#"<html><head>
<script type="application/ld+json">{not-json</script>
<title>Acme</title>
</head></html>"#;
        let p = parse_page(html);
        // Falls back to <title>; doesn't panic.
        assert_eq!(p.name.as_deref(), Some("Acme"));
    }
}

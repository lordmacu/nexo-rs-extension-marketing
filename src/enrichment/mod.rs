//! Web scraper for corporate domain enrichment.
//!
//! Branches off the SDK's `enrichment::cache` (TTL store) +
//! `domain_classifier` (skip personal/disposable). The scraper
//! fetches `https://<domain>/` (and falls back to `/about`),
//! parses meta tags + JSON-LD, returns a `ScrapedPage`.
//! Concurrency-bounded via tokio Semaphore; robots-aware.

pub mod scraper;

pub use scraper::{ScrapedPage, Scraper, ScraperConfig};

//! Five SDK `EnrichmentSource` impls: `display_name_parser`,
//! `signature_parser`, `llm_extractor`, `cross_thread_linker`,
//! `reply_to_reader`. Cheap-first ordering — caller composes
//! them into the chain in priority order.

pub mod cross_thread;
pub mod display_name;
pub mod llm_extractor;
pub mod reply_to;
pub mod signature;

pub use cross_thread::CrossThreadLinker;
pub use display_name::DisplayNameParser;
pub use llm_extractor::{LlmExtractor, LlmExtractorBackend, NoopLlmExtractor};
pub use reply_to::ReplyToReader;
pub use signature::SignatureParser;

//! Email-template builder primitives — block-based composer
//! similar to a stripped-down Elementor for email. Operator
//! authors templates from a fixed set of email-safe blocks
//! (heading, paragraph, button, image, divider, spacer,
//! two-column, list); the renderer emits 600-px-wide
//! table-based HTML with inline CSS that survives the lowest
//! common denominator email client (Outlook's Word renderer +
//! Gmail's mobile webview).
//!
//! Compose flow picks a template, renders with the recipient's
//! variables substituted, and uses the result as the outbound
//! body — same OutboundPublisher path as a manual compose.

pub mod blocks;
pub mod sanitize;
pub mod store;

pub use blocks::{render_block, render_template, EmailBlock, TextAlign};
pub use sanitize::email_safe_html;
pub use store::{
    migrate, EmailTemplate, EmailTemplateStore, TemplateStoreError,
};

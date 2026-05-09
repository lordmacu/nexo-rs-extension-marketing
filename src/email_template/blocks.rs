//! Email-safe block types + table-based renderer.
//!
//! Email clients (especially Outlook's Word renderer + Gmail's
//! mobile webview) ignore most modern CSS. This renderer
//! emits the table-based, inline-CSS markup that survives the
//! lowest common denominator: 600px-wide centered table, role
//! presentation hints for screen readers, web-safe font stack.
//!
//! The renderer is intentionally a pure fn — same blocks +
//! same vars always produce the same HTML — so tests + the
//! frontend's preview pane (which ports the same logic) stay
//! aligned.
//!
//! Variable substitution: `{{name}}` style, looked up in a
//! `HashMap<String, String>`. Unknown placeholders pass
//! through unchanged so a partial render still ships.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

impl Default for TextAlign {
    fn default() -> Self {
        Self::Left
    }
}

impl TextAlign {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
        }
    }
}

/// One block in an email template. Persisted as JSON inside
/// `marketing_email_templates.blocks_json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmailBlock {
    Heading {
        text: String,
        /// 1, 2, 3 — clamped server-side. Anything else falls
        /// back to 2.
        #[serde(default = "default_heading_level")]
        level: u8,
        #[serde(default)]
        color: Option<String>,
        #[serde(default)]
        align: TextAlign,
    },
    Paragraph {
        text: String,
        #[serde(default)]
        color: Option<String>,
        #[serde(default)]
        align: TextAlign,
        /// Px size, clamped 10..=32 server-side.
        #[serde(default = "default_paragraph_size")]
        font_size: u8,
    },
    Button {
        text: String,
        url: String,
        #[serde(default = "default_button_bg")]
        bg_color: String,
        #[serde(default = "default_button_text")]
        text_color: String,
        #[serde(default = "default_align_center")]
        align: TextAlign,
    },
    Image {
        url: String,
        #[serde(default)]
        alt: String,
        /// `None` → 100% (use full container width).
        #[serde(default)]
        width: Option<u32>,
        #[serde(default = "default_align_center")]
        align: TextAlign,
        /// Optional click-through. When set, image becomes a
        /// link.
        #[serde(default)]
        link_url: Option<String>,
    },
    Divider {
        #[serde(default = "default_divider_color")]
        color: String,
    },
    Spacer {
        #[serde(default = "default_spacer_height")]
        height_px: u32,
    },
    TwoColumn {
        #[serde(default)]
        left: Vec<EmailBlock>,
        #[serde(default)]
        right: Vec<EmailBlock>,
    },
    List {
        items: Vec<String>,
        #[serde(default)]
        ordered: bool,
        #[serde(default)]
        color: Option<String>,
    },
}

fn default_heading_level() -> u8 {
    2
}
fn default_paragraph_size() -> u8 {
    16
}
fn default_button_bg() -> String {
    "#4f46e5".into()
}
fn default_button_text() -> String {
    "#ffffff".into()
}
fn default_align_center() -> TextAlign {
    TextAlign::Center
}
fn default_divider_color() -> String {
    "#e5e7eb".into()
}
fn default_spacer_height() -> u32 {
    24
}

/// Render the block array into one self-contained HTML string
/// suitable for an email body. Wraps every render in a 600px
/// centered table — operators don't have to author the
/// boilerplate.
pub fn render_template(blocks: &[EmailBlock], vars: &HashMap<String, String>) -> String {
    let body = blocks.iter().map(|b| render_block(b, vars)).collect::<String>();
    format!(
        r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd"><html xmlns="http://www.w3.org/1999/xhtml"><head><meta http-equiv="Content-Type" content="text/html; charset=UTF-8"/></head><body style="margin:0;padding:0;background:#f5f5f5;"><table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" style="background:#f5f5f5;"><tr><td align="center" style="padding:24px 0;"><table role="presentation" width="600" cellpadding="0" cellspacing="0" border="0" style="background:#ffffff;font-family:-apple-system,BlinkMacSystemFont,Segoe UI,Roboto,Helvetica,Arial,sans-serif;color:#1a1a1a;"><tbody>{body}</tbody></table></td></tr></table></body></html>"#
    )
}

/// Render a single block as a `<tr>...</tr>` (the parent
/// renderer wraps everything in a 600px table). TwoColumn is
/// the one exception — emits its own nested table inside a tr.
pub fn render_block(block: &EmailBlock, vars: &HashMap<String, String>) -> String {
    match block {
        EmailBlock::Heading { text, level, color, align } => {
            let text = substitute_vars(text, vars);
            let text = escape_html(&text);
            let level = match *level {
                1 | 2 | 3 => *level,
                _ => 2,
            };
            let size = match level {
                1 => 32,
                2 => 24,
                _ => 20,
            };
            let color = color.as_deref().unwrap_or("#1a1a1a");
            format!(
                r#"<tr><td style="padding:12px 24px;text-align:{align};"><h{level} style="margin:0;font-size:{size}px;line-height:1.3;color:{color};font-weight:600;">{text}</h{level}></td></tr>"#,
                align = align.as_str(),
            )
        }
        EmailBlock::Paragraph { text, color, align, font_size } => {
            let text = substitute_vars(text, vars);
            let text = escape_html(&text).replace('\n', "<br/>");
            let color = color.as_deref().unwrap_or("#374151");
            let size = (*font_size).clamp(10, 32);
            format!(
                r#"<tr><td style="padding:8px 24px;text-align:{align};"><p style="margin:0;font-size:{size}px;line-height:1.5;color:{color};">{text}</p></td></tr>"#,
                align = align.as_str(),
            )
        }
        EmailBlock::Button { text, url, bg_color, text_color, align } => {
            let text = substitute_vars(text, vars);
            let text = escape_html(&text);
            let url = substitute_vars(url, vars);
            let url_attr = escape_html_attr(&url);
            let bg = sanitize_color(bg_color, "#4f46e5");
            let fg = sanitize_color(text_color, "#ffffff");
            format!(
                r#"<tr><td style="padding:16px 24px;text-align:{align};"><a href="{url_attr}" style="display:inline-block;padding:12px 28px;background:{bg};color:{fg};text-decoration:none;border-radius:6px;font-weight:600;font-size:15px;">{text}</a></td></tr>"#,
                align = align.as_str(),
            )
        }
        EmailBlock::Image { url, alt, width, align, link_url } => {
            let url = substitute_vars(url, vars);
            let url_attr = escape_html_attr(&url);
            let alt = escape_html_attr(alt);
            let width_attr = match width {
                Some(w) => format!(r#" width="{w}" style="max-width:{w}px;width:100%;""#),
                None => r#" style="max-width:100%;""#.to_string(),
            };
            let img_tag = format!(
                r#"<img src="{url_attr}" alt="{alt}"{width_attr}/>"#
            );
            let inner = match link_url {
                Some(link) if !link.is_empty() => {
                    let link = substitute_vars(link, vars);
                    let link_attr = escape_html_attr(&link);
                    format!(r#"<a href="{link_attr}">{img_tag}</a>"#)
                }
                _ => img_tag,
            };
            format!(
                r#"<tr><td style="padding:8px 24px;text-align:{align};">{inner}</td></tr>"#,
                align = align.as_str(),
            )
        }
        EmailBlock::Divider { color } => {
            let color = sanitize_color(color, "#e5e7eb");
            format!(
                r#"<tr><td style="padding:12px 24px;"><hr style="border:0;border-top:1px solid {color};margin:0;"/></td></tr>"#
            )
        }
        EmailBlock::Spacer { height_px } => {
            let h = (*height_px).clamp(4, 200);
            format!(
                r#"<tr><td style="height:{h}px;line-height:{h}px;font-size:0;">&nbsp;</td></tr>"#
            )
        }
        EmailBlock::TwoColumn { left, right } => {
            let left_html = left.iter().map(|b| render_block(b, vars)).collect::<String>();
            let right_html = right.iter().map(|b| render_block(b, vars)).collect::<String>();
            // Two nested tables side by side. `valign="top"` so
            // unequal heights don't push one column to the
            // middle. Width 50% each.
            format!(
                r#"<tr><td style="padding:8px 12px;"><table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0"><tr><td valign="top" width="50%" style="padding:0 12px 0 0;"><table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0"><tbody>{left_html}</tbody></table></td><td valign="top" width="50%" style="padding:0 0 0 12px;"><table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0"><tbody>{right_html}</tbody></table></td></tr></table></td></tr>"#
            )
        }
        EmailBlock::List { items, ordered, color } => {
            let tag = if *ordered { "ol" } else { "ul" };
            let color = color.as_deref().unwrap_or("#374151");
            let list_items = items
                .iter()
                .map(|i| {
                    let i = substitute_vars(i, vars);
                    format!(
                        r#"<li style="margin:4px 0;font-size:16px;line-height:1.5;color:{color};">{}</li>"#,
                        escape_html(&i)
                    )
                })
                .collect::<String>();
            format!(
                r#"<tr><td style="padding:8px 24px 8px 40px;"><{tag} style="margin:0;padding:0 0 0 16px;">{list_items}</{tag}></td></tr>"#
            )
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────

fn substitute_vars(template: &str, vars: &HashMap<String, String>) -> String {
    if !template.contains("{{") {
        return template.to_string();
    }
    let mut out = template.to_string();
    for (k, v) in vars {
        let needle = format!("{{{{{k}}}}}");
        out = out.replace(&needle, v);
    }
    out
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_html_attr(s: &str) -> String {
    escape_html(s).replace('"', "&quot;")
}

/// Defensive color sanitizer — only allows `#rrggbb` /
/// `#rgb` / a small palette of CSS color words. Unknown values
/// fall back to `default` so a malicious template can't inject
/// CSS via the color field.
fn sanitize_color(input: &str, default: &str) -> String {
    let s = input.trim();
    if s.starts_with('#') && s.chars().skip(1).all(|c| c.is_ascii_hexdigit()) {
        let len = s.len() - 1;
        if len == 3 || len == 6 || len == 8 {
            return s.to_string();
        }
    }
    // Allow a tiny named palette commonly used in operator
    // emails. Avoids dragging in a full CSS color crate.
    const NAMED: &[&str] = &[
        "transparent", "white", "black", "red", "green", "blue", "yellow",
        "orange", "purple", "gray", "grey", "silver", "teal", "navy",
    ];
    if NAMED.contains(&s.to_lowercase().as_str()) {
        return s.to_string();
    }
    default.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn heading_renders_with_level_and_color() {
        let b = EmailBlock::Heading {
            text: "Hola Juan".into(),
            level: 1,
            color: Some("#000000".into()),
            align: TextAlign::Center,
        };
        let out = render_block(&b, &HashMap::new());
        assert!(out.contains("<h1"));
        assert!(out.contains("Hola Juan"));
        assert!(out.contains("text-align:center"));
        assert!(out.contains("#000000"));
    }

    #[test]
    fn heading_invalid_level_falls_back_to_h2() {
        let b = EmailBlock::Heading {
            text: "x".into(),
            level: 7,
            color: None,
            align: TextAlign::default(),
        };
        let out = render_block(&b, &HashMap::new());
        assert!(out.contains("<h2"));
    }

    #[test]
    fn variables_substitute_in_text() {
        let b = EmailBlock::Paragraph {
            text: "Hola {{name}}, gracias.".into(),
            color: None,
            align: TextAlign::Left,
            font_size: 16,
        };
        let out = render_block(&b, &vars(&[("name", "Camila")]));
        assert!(out.contains("Hola Camila, gracias."));
    }

    #[test]
    fn unknown_variables_pass_through() {
        let b = EmailBlock::Paragraph {
            text: "Hi {{unknown}} there".into(),
            color: None,
            align: TextAlign::Left,
            font_size: 16,
        };
        let out = render_block(&b, &HashMap::new());
        assert!(out.contains("{{unknown}}"));
    }

    #[test]
    fn html_escapes_user_text() {
        let b = EmailBlock::Heading {
            text: "<script>alert(1)</script>".into(),
            level: 2,
            color: None,
            align: TextAlign::default(),
        };
        let out = render_block(&b, &HashMap::new());
        assert!(!out.contains("<script"));
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn button_renders_as_anchor_tag() {
        let b = EmailBlock::Button {
            text: "Click".into(),
            url: "https://example.com".into(),
            bg_color: "#ff0000".into(),
            text_color: "#ffffff".into(),
            align: TextAlign::Center,
        };
        let out = render_block(&b, &HashMap::new());
        assert!(out.contains("<a "));
        assert!(out.contains("href=\"https://example.com\""));
        assert!(out.contains("background:#ff0000"));
    }

    #[test]
    fn button_url_substitutes_vars() {
        let b = EmailBlock::Button {
            text: "Demo".into(),
            url: "https://app.com/u/{{token}}".into(),
            bg_color: "#000".into(),
            text_color: "#fff".into(),
            align: TextAlign::Left,
        };
        let out = render_block(&b, &vars(&[("token", "abc123")]));
        assert!(out.contains("/u/abc123"));
    }

    #[test]
    fn malicious_color_falls_back_to_default() {
        let b = EmailBlock::Button {
            text: "Click".into(),
            url: "x".into(),
            bg_color: "javascript:alert(1)".into(),
            text_color: "#fff".into(),
            align: TextAlign::Left,
        };
        let out = render_block(&b, &HashMap::new());
        // Default button color is the fallback, not the
        // malicious value.
        assert!(!out.contains("javascript"));
        assert!(out.contains("#4f46e5"));
    }

    #[test]
    fn image_with_link_wraps_in_anchor() {
        let b = EmailBlock::Image {
            url: "https://x.com/banner.png".into(),
            alt: "Banner".into(),
            width: Some(300),
            align: TextAlign::Center,
            link_url: Some("https://example.com/promo".into()),
        };
        let out = render_block(&b, &HashMap::new());
        assert!(out.contains("<a href=\"https://example.com/promo\">"));
        assert!(out.contains("<img src=\"https://x.com/banner.png\""));
        assert!(out.contains("width=\"300\""));
    }

    #[test]
    fn divider_emits_hr() {
        let b = EmailBlock::Divider {
            color: "#cccccc".into(),
        };
        let out = render_block(&b, &HashMap::new());
        assert!(out.contains("<hr"));
        assert!(out.contains("#cccccc"));
    }

    #[test]
    fn spacer_clamps_to_safe_range() {
        let big = EmailBlock::Spacer { height_px: 9999 };
        let small = EmailBlock::Spacer { height_px: 1 };
        let out_big = render_block(&big, &HashMap::new());
        let out_small = render_block(&small, &HashMap::new());
        assert!(out_big.contains("height:200px"));
        assert!(out_small.contains("height:4px"));
    }

    #[test]
    fn two_column_nests_blocks_side_by_side() {
        let b = EmailBlock::TwoColumn {
            left: vec![EmailBlock::Paragraph {
                text: "left side".into(),
                color: None,
                align: TextAlign::Left,
                font_size: 14,
            }],
            right: vec![EmailBlock::Paragraph {
                text: "right side".into(),
                color: None,
                align: TextAlign::Right,
                font_size: 14,
            }],
        };
        let out = render_block(&b, &HashMap::new());
        assert!(out.contains("left side"));
        assert!(out.contains("right side"));
        // Two cells with valign=top.
        assert_eq!(out.matches("valign=\"top\"").count(), 2);
    }

    #[test]
    fn list_renders_ul_or_ol() {
        let ul = EmailBlock::List {
            items: vec!["one".into(), "two".into()],
            ordered: false,
            color: None,
        };
        let ol = EmailBlock::List {
            items: vec!["a".into(), "b".into()],
            ordered: true,
            color: None,
        };
        let ul_out = render_block(&ul, &HashMap::new());
        let ol_out = render_block(&ol, &HashMap::new());
        assert!(ul_out.contains("<ul"));
        assert!(ol_out.contains("<ol"));
        assert_eq!(ul_out.matches("<li").count(), 2);
    }

    #[test]
    fn render_template_wraps_in_600px_table() {
        let blocks = vec![EmailBlock::Heading {
            text: "Hi".into(),
            level: 1,
            color: None,
            align: TextAlign::default(),
        }];
        let out = render_template(&blocks, &HashMap::new());
        assert!(out.contains("<!DOCTYPE"));
        assert!(out.contains("width=\"600\""));
        assert!(out.contains("<h1"));
        assert!(out.contains("Hi"));
    }

    #[test]
    fn json_round_trip_preserves_blocks() {
        let blocks = vec![
            EmailBlock::Heading {
                text: "Title".into(),
                level: 1,
                color: Some("#000".into()),
                align: TextAlign::Center,
            },
            EmailBlock::Spacer { height_px: 16 },
            EmailBlock::Paragraph {
                text: "Body".into(),
                color: None,
                align: TextAlign::Left,
                font_size: 14,
            },
        ];
        let json = serde_json::to_string(&blocks).unwrap();
        let back: Vec<EmailBlock> = serde_json::from_str(&json).unwrap();
        assert_eq!(blocks, back);
    }
}

//! Built-in defaults — keyword + role lists shared by every
//! tenant, plus the threshold sets each `Strictness` preset
//! materializes into.
//!
//! All lists are lowercase and matched substring-wise. Tenant
//! overrides extend (block lists) or contradict (allow lists)
//! these defaults; see `rules::ResolvedRules` for the merge.

use serde::{Deserialize, Serialize};

/// Sensitivity preset — operator picks one in the UI; the
/// resolver materializes it into a `ThresholdSet`. `Custom`
/// means "use the per-tenant `ThresholdSet` stored alongside
/// the preset on `SpamFilterConfig`".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strictness {
    Lax,
    Balanced,
    Strict,
    Custom,
}

impl Default for Strictness {
    fn default() -> Self {
        Self::Balanced
    }
}

impl Strictness {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Lax => "lax",
            Self::Balanced => "balanced",
            Self::Strict => "strict",
            Self::Custom => "custom",
        }
    }

    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "lax" => Some(Self::Lax),
            "balanced" => Some(Self::Balanced),
            "strict" => Some(Self::Strict),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

/// Tunable thresholds the classifier branches on.
/// `Custom` strictness uses operator-supplied values; the three
/// fixed presets snapshot to one of `LAX_THRESHOLDS` /
/// `BALANCED_THRESHOLDS` / `STRICT_THRESHOLDS`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThresholdSet {
    /// Drop messages with `>= 1` image and zero visible text.
    pub image_only_drop: bool,
    /// Drop messages with image-heavy bodies (image-count
    /// threshold + visible-text threshold).
    pub image_heavy_drop: bool,
    pub image_heavy_min_count: u32,
    pub image_heavy_max_text_chars: u32,
    /// Drop when a role-mailbox sender ships any promo keyword.
    pub role_keyword_drop: bool,
    /// Drop when N or more independent weak signals fire.
    pub multi_weak_drop: bool,
    pub multi_weak_threshold: u32,
}

pub const LAX_THRESHOLDS: ThresholdSet = ThresholdSet {
    image_only_drop: true,
    image_heavy_drop: false,
    image_heavy_min_count: 4,
    image_heavy_max_text_chars: 100,
    role_keyword_drop: false,
    multi_weak_drop: true,
    multi_weak_threshold: 3,
};

pub const BALANCED_THRESHOLDS: ThresholdSet = ThresholdSet {
    image_only_drop: true,
    image_heavy_drop: true,
    image_heavy_min_count: 3,
    image_heavy_max_text_chars: 200,
    role_keyword_drop: true,
    multi_weak_drop: true,
    multi_weak_threshold: 2,
};

pub const STRICT_THRESHOLDS: ThresholdSet = ThresholdSet {
    image_only_drop: true,
    image_heavy_drop: true,
    image_heavy_min_count: 2,
    image_heavy_max_text_chars: 500,
    role_keyword_drop: true,
    multi_weak_drop: true,
    multi_weak_threshold: 2,
};

impl Default for ThresholdSet {
    fn default() -> Self {
        BALANCED_THRESHOLDS.clone()
    }
}

impl ThresholdSet {
    pub fn for_preset(s: Strictness) -> Self {
        match s {
            Strictness::Lax => LAX_THRESHOLDS.clone(),
            Strictness::Balanced => BALANCED_THRESHOLDS.clone(),
            Strictness::Strict => STRICT_THRESHOLDS.clone(),
            // `Custom` falls back to balanced when the operator
            // hasn't stored their own row yet — same shape the UI
            // shows on first load.
            Strictness::Custom => BALANCED_THRESHOLDS.clone(),
        }
    }
}

/// Promo / unsubscribe vocabulary — Spanish + English.
/// Lowercase substring match against subject + visible body.
pub const DEFAULT_PROMO_KEYWORDS: &[&str] = &[
    // ── Unsubscribe / footer (very strong) ──────────────────
    "unsubscribe",
    "opt out",
    "opt-out",
    "manage preferences",
    "email preferences",
    "manage subscription",
    "view in browser",
    "view this email in your browser",
    "view email in browser",
    "darse de baja",
    "dar de baja",
    "cancelar suscripción",
    "cancelar suscripcion",
    "no recibir más",
    "no recibir mas",
    "ver en navegador",
    "preferencias de correo",
    // ── Marketing / promo verbs ─────────────────────────────
    "limited time",
    "limited offer",
    "exclusive offer",
    "special offer",
    "flash sale",
    "act now",
    "shop now",
    "buy now",
    "click here",
    "learn more",
    "free shipping",
    "promotion",
    "promotional",
    "discount code",
    "use code",
    "free trial",
    "newsletter",
    "boletín",
    "boletin",
    "oferta especial",
    "oferta exclusiva",
    "descuento exclusivo",
    "promoción",
    "promocion",
    "sólo por hoy",
    "solo por hoy",
    "aprovecha",
    "oportunidad única",
    "oportunidad unica",
    "compra ahora",
    "envío gratis",
    "envio gratis",
    "código de descuento",
    "codigo de descuento",
    "no responda a este correo",
    "do not reply",
    "this is an automated",
];

/// Role-mailbox local-parts. `info@` is intentionally absent —
/// real prospects in LATAM SMBs commonly use `info@<theirdomain>`
/// as a primary mailbox.
pub const DEFAULT_ROLE_LOCAL_PARTS: &[&str] = &[
    "noreply",
    "no-reply",
    "no.reply",
    "donotreply",
    "do-not-reply",
    "notify",
    "notifications",
    "notification",
    "newsletter",
    "newsletters",
    "marketing",
    "promo",
    "promos",
    "promotions",
    "mailer",
    "mail",
    "bulk",
    "campaign",
    "campaigns",
    "news",
    "broadcast",
    "automated",
    "auto",
];

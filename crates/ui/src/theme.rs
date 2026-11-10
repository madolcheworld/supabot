//! Theme tokens: colours, spacing, radii and the type scale.
//!
//! Dark-first (the only variant the app ships). The palette is seeded from the
//! verified spike literals recorded in [`../../../docs/ui.md`] §4 so the colours
//! a reviewer sees match the ones that were proven to render through llvmpipe.
//!
//! No vendor art (D0.3): every "avatar" is a colour tile derived from
//! [`swarmbot_contracts::Bot::effective_color`] plus the bot's initial. Nothing
//! here loads an image or a bundled font — GPUI's `font-kit` resolves a system
//! face, which is why the crate builds with `default-features = false`.

use gpui::{Hsla, Rgba, rgb};
use swarmbot_contracts::Bot;

/// Main pane background (`#ffffff`).
pub const BACKGROUND: u32 = 0xffffff;
/// Sidebar / recessed surface (`#f7f7f8`).
pub const SURFACE: u32 = 0xf7f7f8;
/// A raised card surface, between `SURFACE` and `BACKGROUND` (`#ececee`).
pub const SURFACE_RAISED: u32 = 0xececee;
/// Separators and hairline borders (`#e5e7eb`).
pub const BORDER: u32 = 0xe5e7eb;
/// Primary text (`#111827`).
pub const TEXT: u32 = 0x111827;
/// Secondary text and labels (`#6b7280`).
pub const TEXT_MUTED: u32 = 0x6b7280;
/// Status text, timestamps (`#9ca3af`).
pub const TEXT_DIM: u32 = 0x9ca3af;
/// Primary action / accent (`#2563eb`).
pub const ACCENT: u32 = 0x2563eb;
/// Accent hover, a little darker.
pub const ACCENT_HOVER: u32 = 0x1d4ed8;
/// Destructive / error / denied (`#ef4444`).
pub const DANGER: u32 = 0xef4444;
/// Success / online / allow (`#10b981`).
pub const SUCCESS: u32 = 0x10b981;
/// Warning / queued / expiry soon (`#f59e0b`).
pub const WARNING: u32 = 0xf59e0b;
/// Owner bubble background (solid black `#000000` like in the screenshot).
pub const OWNER_BUBBLE: u32 = 0x000000;
/// Bot bubble background (a clean soft grey `#f3f4f6`).
pub const BOT_BUBBLE: u32 = 0xf3f4f6;
/// Monospace code / tool-output background.
pub const CODE_BG: u32 = 0xf3f4f6;
/// Soft red/blush alert banner background (`#fef2f2`).
pub const NOTICE_ERROR_BG: u32 = 0xfef2f2;
/// Soft red/blush alert banner border (`#fecdd3`).
pub const NOTICE_ERROR_BORDER: u32 = 0xfecdd3;
/// Soft red/blush alert banner text (`#991b1b`).
pub const NOTICE_ERROR_FG: u32 = 0x991b1b;

/// Parse a `#rrggbb` (or `rrggbb`) string into a GPUI [`Rgba`].
///
/// Falls back to the accent colour rather than failing: a bot colour is
/// decoration, and a malformed one must never take the window down. This is the
/// one place the app tolerates bad input silently, and it says so here.
pub fn hex(color: &str) -> Rgba {
    let digits = color.trim().trim_start_matches('#');
    let parsed = u32::from_str_radix(digits, 16);
    match parsed {
        Ok(v) if digits.len() == 6 => rgb(v),
        // Anything else (a name, a short form, garbage) gets the accent.
        _ => rgb(ACCENT),
    }
}

/// The avatar tile colour for a bot, from its [`Bot::effective_color`].
pub fn bot_color(bot: &Bot) -> Rgba {
    hex(&bot.effective_color())
}

/// The avatar tile colour for a bare bot id when the roster is not loaded yet,
/// so a bubble still gets a deterministic tint instead of a default.
pub fn bot_color_from_id(id: &str) -> Rgba {
    // Mirror of `Bot::effective_color`'s FNV-1a → hue mapping, kept here so a
    // colour can be derived before the roster snapshot arrives.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in id.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let hue = (hash % 360) as f32;
    hsl_to_rgba(hue, 0.55, 0.55)
}

/// HSL → RGBA, matching the contracts' `hsl_to_rgb` so a tile derived from an id
/// equals the tile the roster would give the same bot.
fn hsl_to_rgba(h: f32, s: f32, l: f32) -> Rgba {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgba {
        r: to(r1) as f32 / 255.0,
        g: to(g1) as f32 / 255.0,
        b: to(b1) as f32 / 255.0,
        a: 1.0,
    }
}

/// A colour as an [`Hsla`] for `.blend()`/opacity work.
pub fn hsla(color: u32) -> Hsla {
    rgb(color).into()
}

/// Blend a colour toward black by `amount` (0..1) for hover/active states that
/// should read as "the same colour, pressed".
pub fn darken(color: Rgba, amount: f32) -> Rgba {
    let k = (1.0 - amount).clamp(0.0, 1.0);
    Rgba {
        r: color.r * k,
        g: color.g * k,
        b: color.b * k,
        a: color.a,
    }
}

/// Spacing scale (pixels). GPUI's tailwind-style `_1.._12` are 4px steps; these
/// named constants keep the handful of off-scale values we use in one place.
pub mod space {
    use gpui::{Pixels, px};
    pub const XS: Pixels = px(4.0);
    pub const SM: Pixels = px(8.0);
    pub const MD: Pixels = px(12.0);
    pub const LG: Pixels = px(16.0);
    pub const SIDEBAR_W: Pixels = px(280.0);
    pub const GRAPH_W: Pixels = px(320.0);
    pub const AVATAR: Pixels = px(32.0);
    pub const HEADER_H: Pixels = px(56.0);
}

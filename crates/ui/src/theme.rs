//! Theme tokens: colours, spacing, radii and the type scale.
//!
//! **One light theme**, which is the only variant the app ships: `BACKGROUND` is
//! `#ffffff`, `TEXT` is `#111827`, the sidebar surface is `#f7f7f8` at
//! `SIDEBAR_W = 280`, and the header is `HEADER_H = 56`. This file used to say
//! "dark-first" while the tokens had already gone light — a header comment that
//! contradicts the constants below it is the kind of drift a reader trusts and
//! then gets bitten by, so the comment states the palette the code actually has
//! and [`../../../docs/ui.md`] §4 records the same values. A dark variant would
//! be a real change here (a `Theme` value threaded through every view, not a
//! constant), and nobody has asked for one.
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
/// Primary text (`#111827`). 16.0:1 on `BACKGROUND`.
pub const TEXT: u32 = 0x111827;
/// Secondary text and labels (`#4b5563`). 7.6:1 on `BACKGROUND`.
///
/// Darker than it used to be (`#6b7280`), not for taste: the *third* tier below
/// was `#9ca3af`, which is **2.5:1** on white — under WCAG AA's 4.5:1 for body
/// text, and it was the colour of every timestamp, section heading, preview line
/// and helper sentence in the app. Meeting 4.5:1 needs `#6b7280`, so that value
/// moved down a tier and this one moved darker to keep three readable levels
/// instead of two-and-a-half. [`tests::every_text_token_is_legible_on_its_surface`]
/// computes the ratios from these constants, so the palette cannot quietly drift
/// back under the line.
pub const TEXT_MUTED: u32 = 0x4b5563;
/// The faintest text tier: timestamps, metadata, section labels (`#5f6976`).
/// 5.57:1 on `BACKGROUND`, 4.72:1 on the darkest surface it is drawn on
/// (`SURFACE_RAISED`) — that last number is the one that sets the value, because
/// timestamps sit inside raised cards as well as on white.
pub const TEXT_DIM: u32 = 0x5f6976;
/// Primary action / accent (`#1d4ed8`).
///
/// One step darker than the `#2563eb` this used to be, because the accent is
/// used as *text* as well as a fill ("sent · addressed" in the top bar) and
/// `#2563eb` is 4.38:1 on `SURFACE_RAISED` — under the bar by a hair. At
/// `#1d4ed8` it is 5.68:1 there and 6.70:1 on white, and white-on-accent (the
/// primary button) improves from 5.17:1 to 6.70:1 in the same move.
pub const ACCENT: u32 = 0x1d4ed8;
/// Accent hover, a little darker (`#1e3a8a`).
pub const ACCENT_HOVER: u32 = 0x1e3a8a;
/// Destructive / error / denied (`#ef4444`). A **fill** colour: 3.5:1 on white,
/// which is fine for a dot, a border or a button face and not enough for text.
pub const DANGER: u32 = 0xef4444;
/// Success / online / allow (`#10b981`). A fill colour (2.4:1 on white).
pub const SUCCESS: u32 = 0x10b981;
/// Warning / queued / expiry soon (`#f59e0b`). A fill colour (2.1:1 on white).
pub const WARNING: u32 = 0xf59e0b;
/// The same three hues as **text**, darkened until each clears 4.5:1 on the light
/// surfaces they are drawn on (5.5:1, 7.1:1 and 5.9:1 respectively).
///
/// The vivid values above are right for a 9px status dot and wrong for the word
/// next to it: "connected" in `#10b981` is 2.4:1, so the app's own health
/// indicator was the least legible text on the row. Two tokens instead of one is
/// the honest split — a hue you can see at 9px and a hue you can read at 12px are
/// different colours, and pretending otherwise is how a palette ends up with
/// text nobody can read.
pub const DANGER_TEXT: u32 = 0xb91c1c;
/// See [`DANGER_TEXT`].
pub const SUCCESS_TEXT: u32 = 0x047857;
/// See [`DANGER_TEXT`].
pub const WARNING_TEXT: u32 = 0x92400e;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.x relative luminance for an `0xrrggbb` colour: sRGB channels are
    /// linearised, then weighted 0.2126/0.7152/0.0722.
    fn luminance(color: u32) -> f64 {
        let channel = |shift: u32| {
            let v = ((color >> shift) & 0xff) as f64 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let (r, g, b) = (channel(16), channel(8), channel(0));
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn contrast(fg: u32, bg: u32) -> f64 {
        let (a, b) = (luminance(fg), luminance(bg));
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// The table is the promise: every token the views use for **text** must
    /// clear the 4.5:1 body-text bar on every surface it is drawn on. It did
    /// not — `TEXT_DIM` was `#9ca3af`, which is 2.5:1 on white and was the
    /// colour of every timestamp, preview line and section heading in the app.
    /// A reader who cannot read a timestamp is as blocked as one who cannot
    /// read the message, so this is a defect with a number attached, not taste.
    #[test]
    fn every_text_token_is_legible_on_its_surface() {
        let surfaces = [
            ("BACKGROUND", BACKGROUND),
            ("SURFACE", SURFACE),
            ("SURFACE_RAISED", SURFACE_RAISED),
            ("BOT_BUBBLE", BOT_BUBBLE),
        ];
        let tokens = [
            ("TEXT", TEXT),
            ("TEXT_MUTED", TEXT_MUTED),
            ("TEXT_DIM", TEXT_DIM),
            ("ACCENT", ACCENT),
            ("ACCENT_HOVER", ACCENT_HOVER),
            ("DANGER_TEXT", DANGER_TEXT),
            ("SUCCESS_TEXT", SUCCESS_TEXT),
            ("WARNING_TEXT", WARNING_TEXT),
        ];
        for (token_name, token) in tokens {
            for (surface_name, surface) in surfaces {
                let ratio = contrast(token, surface);
                assert!(
                    ratio >= 4.5,
                    "{token_name} on {surface_name} is {ratio:.2}:1, under the 4.5:1 body-text bar"
                );
            }
        }
    }

    /// The vivid status hues are fills (a 9px dot, a button face), not text: they
    /// are *allowed* to fail the text bar, and this test says so out loud so
    /// nobody "fixes" the split by darkening the dot until it disappears. What it
    /// does require is that each fill hue is distinguishable from its own surface —
    /// a status dot that cannot be seen is not a status.
    #[test]
    fn status_fills_are_visible_even_though_they_are_not_body_text() {
        for (name, fill) in [
            ("DANGER", DANGER),
            ("SUCCESS", SUCCESS),
            ("WARNING", WARNING),
        ] {
            let ratio = contrast(fill, BACKGROUND);
            assert!(
                ratio >= 1.8,
                "{name} fill on BACKGROUND is {ratio:.2}:1 — invisible as a dot"
            );
            // …and each fill has a readable text twin for the word next to it.
            assert!(
                contrast(fill, BACKGROUND) < 4.5,
                "{name} passes the text bar on its own; the *_TEXT twin would be dead weight"
            );
        }
    }

    /// Three text tiers that are actually different: a palette whose "muted" and
    /// "dim" are the same value is a palette with two tiers pretending to have
    /// three, which is how a hierarchy quietly disappears.
    #[test]
    fn the_three_text_tiers_are_ordered_and_distinct() {
        let text = luminance(TEXT);
        let muted = luminance(TEXT_MUTED);
        let dim = luminance(TEXT_DIM);
        assert!(text < muted, "TEXT must be darker than TEXT_MUTED");
        assert!(muted < dim, "TEXT_MUTED must be darker than TEXT_DIM");
        assert!(
            dim - muted > 0.02,
            "TEXT_MUTED and TEXT_DIM are within {:.3} luminance of each other; \
             they have to be distinguishable or one of them is a lie",
            dim - muted
        );
    }
}

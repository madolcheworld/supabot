//! Small shared widgets: avatar tiles, status dots, badges, buttons and the
//! spinner. These are pure element builders — they return a styled [`Div`] the
//! caller chains `.id(..)`/`.on_click(..)` onto when it needs to be interactive,
//! so no click target is created without an id (GPUI requires one).
//!
//! No vendor art (D0.3): an "avatar" is a colour tile from
//! [`swarmbot_contracts::Bot::effective_color`] with the bot's initial on it.

use gpui::{Div, FontWeight, Pixels, Rgba, SharedString, div, prelude::*, px, rgb};

use crate::theme;

/// A button's visual tone.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The primary action (accent filled).
    Primary,
    /// A neutral, recessed action.
    Ghost,
    /// A destructive action.
    Danger,
    /// A permissive/positive action.
    Success,
}

impl Tone {
    /// `(fill, label)` for a tone.
    ///
    /// Every pair here has to clear 4.5:1 between `fill` and `label`, because a
    /// button's whole job is to be read. Two of them did not: white on
    /// `#ef4444` is 3.76:1 (every destructive button in the app), and the
    /// ghost fill `#ececee` on the `#f7f7f8` sidebar is 1.09:1 — the button had
    /// no perceptible edge at all, which is why the sidebar's nav row read as
    /// loose words rather than as buttons.
    fn colors(self) -> (u32, u32) {
        match self {
            Tone::Primary => (theme::ACCENT, 0xffffff),
            Tone::Ghost => (theme::SURFACE_RAISED, theme::TEXT),
            Tone::Danger => (theme::DANGER_TEXT, 0xffffff),
            Tone::Success => (theme::SUCCESS_TEXT, 0xffffff),
        }
    }
}

/// A clickable button base. Add `.id(..).on_click(..)` to make it live; a
/// `disabled` button is dimmed and the caller should not attach a handler.
pub fn button(label: &str, tone: Tone) -> Div {
    let (bg, fg) = tone.colors();
    div()
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .text_sm()
        .cursor_pointer()
        // A ghost fill is barely darker than the surface it sits on, so the
        // boundary has to come from a border or the button is only words.
        .when(tone == Tone::Ghost, |d| {
            d.border_1().border_color(rgb(theme::BORDER))
        })
        .child(label.to_string())
}

/// A dimmed, non-interactive button with a reason shown as its label.
pub fn disabled_button(label: &str) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(theme::SURFACE_RAISED))
        .text_color(rgb(theme::TEXT_DIM))
        .text_sm()
        .opacity(0.6)
        .child(label.to_string())
}

/// An avatar tile: a rounded squircle with cute bot eyes or initials.
pub fn avatar(color: Rgba, initial: char, size: Pixels) -> Div {
    let font = size * 0.46;
    let eye_size = (size * 0.12).max(px(3.0));
    let eye_gap = (size * 0.16).max(px(4.0));

    if initial == 'X' || initial == 'x' {
        div()
            .size(size)
            .flex_none()
            .rounded_full()
            .bg(rgb(0x111111))
            .flex()
            .items_center()
            .justify_center()
            .text_size(font)
            .font_weight(FontWeight::BOLD)
            .text_color(rgb(0xffffff))
            .child("✕")
    } else if initial == '?' {
        div()
            .size(size)
            .flex_none()
            .rounded_xl()
            .bg(color)
            .flex()
            .items_center()
            .justify_center()
            .text_size(font)
            .font_weight(FontWeight::BOLD)
            .text_color(rgb(0xffffff))
            .child("?")
    } else {
        // Cute bot squircle face with two bright white cartoon eyes
        div()
            .size(size)
            .flex_none()
            .rounded_xl()
            .bg(color)
            .flex()
            .items_center()
            .justify_center()
            .gap(eye_gap)
            .child(div().size(eye_size).rounded_full().bg(rgb(0xffffff)))
            .child(div().size(eye_size).rounded_full().bg(rgb(0xffffff)))
    }
}

/// A computer monitor icon for the bot screen card.
pub fn monitor_icon() -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .child(
            div()
                .w(px(32.0))
                .h(px(22.0))
                .rounded_sm()
                .border_2()
                .border_color(rgb(0x9ca3af))
                .bg(rgb(0xffffff)),
        )
        .child(div().w(px(4.0)).h(px(4.0)).bg(rgb(0x9ca3af)))
        .child(
            div()
                .w(px(16.0))
                .h(px(2.0))
                .rounded_full()
                .bg(rgb(0x9ca3af)),
        )
}

/// A circular green checkmark/clock icon for routines.
pub fn routine_icon() -> Div {
    div()
        .size(px(16.0))
        .flex_none()
        .rounded_full()
        .border_1()
        .border_color(rgb(0x10b981))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(9.0))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(0x10b981))
        .child("✓")
}

/// A small round status dot (online/offline, presence).
pub fn dot(color: u32, size: Pixels) -> Div {
    div().size(size).flex_none().rounded_full().bg(rgb(color))
}

/// An icon representing the right sidebar panel (box layout with a right pane).
pub fn sidebar_icon(open: bool) -> Div {
    div()
        .w(px(16.0))
        .h(px(14.0))
        .rounded(px(2.5))
        .border_1()
        .border_color(rgb(theme::TEXT_MUTED))
        .flex()
        .flex_row()
        .overflow_hidden()
        .child(div().flex_1().h_full().bg(rgb(0xffffff)))
        .child(div().w(px(1.0)).h_full().bg(rgb(theme::BORDER)))
        .child(
            div()
                .w(px(4.0))
                .h_full()
                .when(open, |d| d.bg(rgb(theme::TEXT_MUTED)))
                .when(!open, |d| d.bg(rgb(theme::SURFACE))),
        )
}

/// A pill badge (an unread count, a capability count, a status chip).
pub fn badge(label: impl Into<SharedString>, bg: u32, fg: u32) -> Div {
    div()
        .flex_none()
        .px_1()
        .rounded_full()
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .text_xs()
        .child(label.into())
}

/// A spinner glyph that advances with `phase` (braille, so it reads as motion).
/// The app only advances the phase while something is actually working, so an
/// idle window does not redraw for nothing.
pub fn spinner(phase: usize) -> Div {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let glyph = FRAMES[phase % FRAMES.len()];
    div()
        .flex_none()
        .text_size(px(13.0))
        .text_color(rgb(theme::ACCENT_HOVER))
        .child(glyph.to_string())
}

/// A dim uppercase section label ("BOTS", "CONVERSATIONS", "NODES").
pub fn section_label(text: &str) -> Div {
    div()
        .text_xs()
        .text_color(rgb(theme::TEXT_DIM))
        .child(text.to_string())
}

/// The width of a [`kv`] label column.
///
/// 120px was too narrow for the labels this app actually uses: "declared /
/// approved" and "target starts with" are both wider than that at 14px, so they
/// ran into the value column and the row read as one run-on string. 150px holds
/// them, and both cells now truncate, so a future label cannot collide again.
pub const KV_LABEL_W: Pixels = px(150.0);

/// A `label: value` row for the nodes/settings panels.
pub fn kv(label: &str, value: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_baseline()
        .gap_2()
        .text_sm()
        .child(
            div()
                .w(KV_LABEL_W)
                .flex_none()
                .min_w_0()
                .truncate()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(rgb(theme::TEXT))
                .child(value.into()),
        )
}

/// The first display character of a label, for an avatar tile. Falls back to '?'
/// for an empty label rather than panicking.
pub fn initial(label: &str) -> char {
    label
        .trim()
        .chars()
        .next()
        .map(|c| c.to_uppercase().next().unwrap_or(c))
        .unwrap_or('?')
}

/// The title row every non-conversation screen wears: one height, one padding,
/// one divider.
///
/// The five screens each open-coded this row and disagreed: the conversation
/// header and the side panel are `HEADER_H` (56px) with a bottom border, while
/// Nodes/Settings/Routines/Rules/Usage grew to whatever `px_4 py_2` plus a
/// `text_lg` line came to (~39px) and two of them drew no divider at all. Moving
/// between screens therefore shifted every row below the title up or down by 17px
/// and sometimes removed a separator — the kind of thing that reads as "the app
/// jumped" rather than as a style choice. One builder, so the next screen cannot
/// invent a sixth height.
pub fn screen_header(title: &str, subtitle: Option<&str>, actions: Div) -> Div {
    div()
        .h(theme::space::HEADER_H)
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .px_4()
        .border_b_1()
        .border_color(rgb(theme::BORDER))
        .child(
            div()
                .flex()
                .items_baseline()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(theme::TEXT))
                        .truncate()
                        .child(title.to_string()),
                )
                .children(subtitle.map(|s| {
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(s.to_string())
                })),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap_2()
                .child(actions),
        )
}

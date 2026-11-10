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
    fn colors(self) -> (u32, u32) {
        match self {
            Tone::Primary => (theme::ACCENT, 0xffffff),
            Tone::Ghost => (theme::SURFACE_RAISED, theme::TEXT),
            Tone::Danger => (theme::DANGER, 0xffffff),
            Tone::Success => (theme::SUCCESS, 0x0b1410),
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

/// A circular user avatar with initials (e.g. "GT").
pub fn user_avatar(initials: &str, size: Pixels) -> Div {
    div()
        .size(size)
        .flex_none()
        .rounded_full()
        .bg(rgb(0xe5e7eb))
        .flex()
        .items_center()
        .justify_center()
        .text_size(size * 0.44)
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(0x374151))
        .child(initials.to_string())
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

/// A `label: value` row for the nodes/settings panels.
pub fn kv(label: &str, value: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_baseline()
        .gap_2()
        .text_sm()
        .child(
            div()
                .w(px(120.0))
                .flex_none()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
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

//! The left sidebar: the roster, the conversations, a compact node list, and the
//! navigation buttons (Nodes screen, Settings, New group).
//!
//! A bot row is a colour tile (from [`swarmbot_contracts::Bot::effective_color`],
//! never vendor art — D0.3), its label, a one-line status from
//! [`swarmbot_contracts::BotStatus`], and a spinner while it is working. A
//! conversation row shows its title, a one-line preview and an unread badge.

use std::sync::mpsc::Sender;

use gpui::{Entity, SharedString, div, prelude::*, px, rgb};
use swarmbot_contracts::{BotId, BotStatus};

use crate::proto::Command;
use crate::state::{AppState, Screen};
use crate::theme;
use crate::widgets::bits::{self, Tone};

pub fn render(s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    div()
        .w(theme::space::SIDEBAR_W)
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::SURFACE))
        .border_r_1()
        .border_color(rgb(theme::BORDER))
        .child(
            div()
                .px_3()
                .py_2()
                .text_sm()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::TEXT))
                .child("SWARMBOT"),
        )
        // Scrollable middle: bots, conversations, nodes.
        .child(
            div()
                .id("sidebar-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_3()
                .px_2()
                .py_2()
                .child(bots_section(s, state))
                .child(conversations_section(s, state))
                .child(nodes_section(s, state)),
        )
        // Nav buttons pinned to the bottom.
        .child(nav(s, state, commands))
}

fn section(label: &str) -> gpui::Div {
    div().px_1().child(bits::section_label(label))
}

fn bots_section(s: &AppState, state: &Entity<AppState>) -> gpui::Div {
    let mut list = div().flex().flex_col().gap_0p5().child(section("BOTS"));
    for bot in &s.bots {
        let (status_text, working, color) = status_line(&bot.status);
        let id = bot.id.clone();
        let state_h = state.clone();
        let dm = bot.direct_conversation_id.clone();
        let selected = s.open_conversation.as_ref() == Some(&dm);
        list = list.child(
            div()
                .id(SharedString::from(format!("bot-{}", bot.id)))
                .flex()
                .items_center()
                .gap_2()
                .px_1()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(selected, |d| d.bg(rgb(theme::SURFACE_RAISED)))
                .child(bits::avatar(
                    theme::bot_color(bot),
                    bits::initial(&bot.label),
                    theme::space::AVATAR,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(theme::TEXT))
                                        .truncate()
                                        .child(bot.label.clone()),
                                )
                                .when(!bot.skills.is_empty(), |d| {
                                    d.child(bits::badge(
                                        format!(
                                            "{} skill{}",
                                            bot.skills.len(),
                                            if bot.skills.len() > 1 { "s" } else { "" }
                                        ),
                                        theme::SURFACE_RAISED,
                                        theme::TEXT_MUTED,
                                    ))
                                }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(color))
                                .truncate()
                                .child(status_text),
                        ),
                )
                .when(working, |d| d.child(bits::spinner(s.spinner_phase)))
                .on_click(move |_, _, cx: &mut gpui::App| {
                    let dm = dm.clone();
                    state_h.update(cx, |st, cx| {
                        st.open(dm);
                        cx.notify();
                    });
                    let _ = id;
                }),
        );
    }
    list
}

fn conversations_section(s: &AppState, state: &Entity<AppState>) -> gpui::Div {
    let mut list = div()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(section("CONVERSATIONS"));
    for conversation in &s.conversations {
        let id = conversation.id.clone();
        let title = s.conversation_title(conversation);
        let preview = s.preview(&conversation.id).unwrap_or_default();
        let unread = s.unread(&conversation.id);
        let selected = s.open_conversation.as_ref() == Some(&id);
        let state_h = state.clone();
        let open_id = id.clone();
        list = list.child(
            div()
                .id(SharedString::from(format!("cnv-{id}")))
                .flex()
                .items_center()
                .gap_2()
                .px_1()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(selected, |d| d.bg(rgb(theme::SURFACE_RAISED)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(theme::TEXT))
                                .truncate()
                                .child(title),
                        )
                        .when(!preview.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::TEXT_DIM))
                                    .truncate()
                                    .child(preview.clone()),
                            )
                        }),
                )
                .when(unread > 0, |d| {
                    d.child(bits::badge(unread.to_string(), theme::ACCENT, 0xffffff))
                })
                .on_click(move |_, _, cx: &mut gpui::App| {
                    let open_id = open_id.clone();
                    state_h.update(cx, |st, cx| {
                        st.open(open_id);
                        cx.notify();
                    });
                }),
        );
    }
    list
}

fn nodes_section(s: &AppState, state: &Entity<AppState>) -> gpui::Div {
    let mut list = div().flex().flex_col().gap_0p5().child(section("NODES"));
    if s.nodes.is_empty() {
        list = list.child(
            div()
                .px_1()
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .child("no nodes paired"),
        );
    }
    for node in &s.nodes {
        let state_h = state.clone();
        let caps = node.capabilities();
        list = list.child(
            div()
                .id(SharedString::from(format!("node-{}", node.id)))
                .flex()
                .items_center()
                .gap_2()
                .px_1()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .child(bits::dot(
                    if node.online {
                        theme::SUCCESS
                    } else {
                        theme::TEXT_DIM
                    },
                    px(8.0),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_sm()
                        .text_color(rgb(theme::TEXT))
                        .truncate()
                        .child(node.label.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(format!("{} caps", caps.as_slice().len())),
                )
                .on_click(move |_, _, cx: &mut gpui::App| {
                    state_h.update(cx, |st, cx| {
                        st.screen = Screen::Nodes;
                        cx.notify();
                    });
                }),
        );
    }
    list
}

fn nav(s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    let commands_nodes = commands.clone();
    let commands_routines = commands.clone();
    let commands_settings = commands.clone();
    let commands_group = commands.clone();
    let state_nodes = state.clone();
    let state_routines = state.clone();
    let state_settings = state.clone();
    let state_group = state.clone();

    let nav_button = |label: &str, active: bool, tone: Tone| {
        let b = bits::button(label, tone);
        if active {
            b.border_1().border_color(rgb(theme::ACCENT))
        } else {
            b
        }
    };

    div()
        .flex_none()
        .flex()
        .flex_wrap()
        .gap_1()
        .p_2()
        .border_t_1()
        .border_color(rgb(theme::BORDER))
        .child(
            nav_button("Nodes", s.screen == Screen::Nodes, Tone::Ghost)
                .id("nav-nodes")
                .on_click(move |_, _, cx: &mut gpui::App| {
                    state_nodes.update(cx, |st, cx| {
                        st.screen = Screen::Nodes;
                        cx.notify();
                    });
                    let _ = commands_nodes.send(Command::FetchNodes);
                }),
        )
        .child(
            nav_button("Routines", s.screen == Screen::Routines, Tone::Ghost)
                .id("nav-routines")
                .on_click(move |_, _, cx: &mut gpui::App| {
                    state_routines.update(cx, |st, cx| {
                        st.screen = Screen::Routines;
                        cx.notify();
                    });
                    let _ = commands_routines.send(Command::FetchRoutines);
                }),
        )
        .child(
            nav_button("Settings", s.screen == Screen::Settings, Tone::Ghost)
                .id("nav-settings")
                .on_click(move |_, _, cx: &mut gpui::App| {
                    state_settings.update(cx, |st, cx| {
                        st.screen = Screen::Settings;
                        cx.notify();
                    });
                    let _ = commands_settings.send(Command::FetchSettings);
                    let _ = commands_settings.send(Command::FetchBackends);
                }),
        )
        .child(
            nav_button("New group", false, Tone::Primary)
                .id("nav-new-group")
                .on_click(move |_, _, cx: &mut gpui::App| {
                    // One-click group of the roster, capped at the group max. A
                    // member-picker is a future refinement; this creates a real
                    // group so the button is never a no-op that looks like success.
                    let members: Vec<BotId> = {
                        let st = state_group.read(cx);
                        st.bots
                            .iter()
                            .take(swarmbot_contracts::MAX_GROUP_MEMBERS)
                            .map(|b| b.id.clone())
                            .collect()
                    };
                    if members.len() >= swarmbot_contracts::MIN_GROUP_MEMBERS {
                        let lead = members.first().cloned();
                        let _ = commands_group.send(Command::CreateGroup {
                            title: "New group".to_string(),
                            members,
                            lead,
                        });
                    } else {
                        state_group.update(cx, |st, cx| {
                            st.toast = Some("a group needs at least 2 bots".to_string());
                            cx.notify();
                        });
                    }
                }),
        )
}

/// The one-line status text, whether it is "working", and its colour.
fn status_line(status: &BotStatus) -> (String, bool, u32) {
    match status {
        BotStatus::Idle => ("idle".to_string(), false, theme::TEXT_DIM),
        BotStatus::Working { activity, .. } => (activity.clone(), true, theme::ACCENT_HOVER),
        BotStatus::Queued { count } => (format!("{count} queued"), false, theme::WARNING),
        BotStatus::Error { reason } => (reason.clone(), false, theme::DANGER),
        BotStatus::Disabled => ("disabled".to_string(), false, theme::TEXT_DIM),
    }
}

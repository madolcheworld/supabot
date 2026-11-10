//! The left sidebar: Search bar, bot conversations list, unread indicators,
//! plugins, and user profile footer matching the design.

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
        // Top search bar and new button
        .child(header_search(state, commands))
        // Scrollable bot conversations list
        .child(
            div()
                .id("sidebar-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_2()
                .child(bots_list(s, state))
                .child(more_unreads_pill()),
        )
        // Footer: Plugins + User Profile + Quick navigation
        .child(footer(s, state, commands))
}

fn header_search(state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    let state_group = state.clone();
    let commands_group = commands.clone();
    div()
        .flex_none()
        .px_3()
        .pt_3()
        .pb_2()
        .flex()
        .items_center()
        .gap_2()
        .child(
            // Search pill box
            div()
                .flex_1()
                .h(px(36.0))
                .rounded_xl()
                .bg(rgb(0xebebee))
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                .text_color(rgb(theme::TEXT_MUTED))
                .text_sm()
                .child("🔍")
                .child(div().text_color(rgb(theme::TEXT_MUTED)).child("Search")),
        )
        .child(
            // Plus button for new group / conversation
            div()
                .id("nav-new-group")
                .size(px(36.0))
                .rounded_xl()
                .bg(rgb(0xebebee))
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(theme::TEXT))
                .text_lg()
                .font_weight(gpui::FontWeight::MEDIUM)
                .child("+")
                .on_click(move |_, _, cx: &mut gpui::App| {
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

fn bots_list(s: &AppState, state: &Entity<AppState>) -> gpui::Div {
    let mut list = div().flex().flex_col().gap_1();

    // If real bots exist from brain/roster, render them with the redesigned styling
    if !s.bots.is_empty() {
        for (i, bot) in s.bots.iter().enumerate() {
            let (status_text, working, _color) = status_line(&bot.status);
            let dm = bot.direct_conversation_id.clone();
            let selected = s.open_conversation.as_ref() == Some(&dm)
                || (s.open_conversation.is_none() && i == 0);
            let state_h = state.clone();
            let dm_c = dm.clone();
            let preview = s.preview(&dm).unwrap_or(status_text);
            let unread = s.unread(&dm);

            list = list.child(bot_item(
                &format!("bot-{}", bot.id),
                &bot.label,
                theme::bot_color(bot),
                bits::initial(&bot.label),
                "11:40 AM",
                &preview,
                unread > 0,
                selected,
                working,
                s.spinner_phase,
                move |cx| {
                    let dm = dm_c.clone();
                    state_h.update(cx, |st, cx| {
                        st.open(dm);
                        cx.notify();
                    });
                },
            ));
        }
    } else {
        // Sample bots matching the provided screenshot
        let samples = [
            ("Waypoint", "#8b5cf6", 'W', "11:40 AM", "hello", false, true),
            ("Alfred", "#f97316", 'A', "6:33 AM", "Hello", false, false),
            ("Parcel", "#ef4444", 'P', "6:19 AM", "Hello", false, false),
            (
                "Pulse",
                "#18181b",
                'X',
                "6:01 AM",
                "Messaged Alfred: Got it. Added ...",
                false,
                false,
            ),
            (
                "Capstone Client Com...",
                "#ec4899",
                'C',
                "12:15 PM",
                "Afternoon Sarah check: 8 call...",
                true,
                false,
            ),
            (
                "Credit Card Max",
                "#06b6d4",
                'M',
                "Yesterday",
                "It's on. Next run is October 1 at ...",
                false,
                false,
            ),
            (
                "Capstone Firm Ops",
                "#f97316",
                'F',
                "Yesterday",
                "Billing: Sep send/pay unknow...",
                true,
                false,
            ),
            (
                "Gridiron",
                "#10b981",
                'G',
                "4:16 AM",
                "You're in both games now — MI...",
                false,
                false,
            ),
            (
                "New Bot",
                "#a855f7",
                'N',
                "Yesterday",
                "Message from Gridiron: MID ...",
                true,
                false,
            ),
            (
                "Car...",
                "#78716c",
                'C',
                "Yesterday",
                "Pos...",
                false,
                false,
            ),
        ];

        for (i, (name, color_hex, initial, time, preview, unread, active)) in
            samples.iter().enumerate()
        {
            let state_h = state.clone();
            let name_str = name.to_string();
            let selected = if s.open_conversation.is_none() {
                *active
            } else {
                false
            };

            list = list.child(bot_item(
                &format!("mock-bot-{i}"),
                name,
                theme::hex(color_hex),
                *initial,
                time,
                preview,
                *unread,
                selected,
                false,
                0,
                move |cx| {
                    let n = name_str.clone();
                    state_h.update(cx, |st, cx| {
                        st.toast = Some(format!("Selected {}", n));
                        cx.notify();
                    });
                },
            ));
        }
    }

    list
}

#[allow(clippy::too_many_arguments)]
fn bot_item(
    id: &str,
    label: &str,
    color: gpui::Rgba,
    initial: char,
    time: &str,
    preview: &str,
    unread: bool,
    selected: bool,
    working: bool,
    spinner_phase: usize,
    on_click: impl Fn(&mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id.to_string()))
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .rounded_2xl()
        .cursor_pointer()
        .when(selected, |d| d.bg(rgb(theme::SURFACE_RAISED)))
        .hover(|h| h.bg(rgb(0xebebee)))
        .child(bits::avatar(color, initial, px(38.0)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_col()
                .gap_0p5()
                // Row 1: Name + Time
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(theme::TEXT))
                                .truncate()
                                .child(label.to_string()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_DIM))
                                .child(time.to_string()),
                        ),
                )
                // Row 2: Preview + Unread dot
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .truncate()
                                .child(preview.to_string()),
                        )
                        .when(unread, |d| {
                            d.child(
                                div()
                                    .size(px(7.0))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(rgb(0x2563eb)),
                            )
                        })
                        .when(working, |d| d.child(bits::spinner(spinner_phase))),
                ),
        )
        .on_click(move |_, _, cx: &mut gpui::App| {
            on_click(cx);
        })
}

fn more_unreads_pill() -> gpui::Div {
    div().flex().justify_center().py_2().child(
        div()
            .px_4()
            .py_1p5()
            .rounded_full()
            .bg(rgb(0x1d4ed8))
            .text_color(rgb(0xffffff))
            .text_xs()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .flex()
            .items_center()
            .gap_1p5()
            .cursor_pointer()
            .hover(|h| h.bg(rgb(0x1e40af)))
            .child("↓")
            .child("More unreads"),
    )
}

fn footer(_s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    let state_settings = state.clone();
    let state_nodes = state.clone();
    let state_routines = state.clone();
    let commands_settings = commands.clone();
    let commands_nodes = commands.clone();
    let commands_routines = commands.clone();

    div()
        .flex_none()
        .border_t_1()
        .border_color(rgb(theme::BORDER))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        // Plugins button row
        .child(
            div()
                .id("sidebar-plugins")
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1p5()
                .rounded_xl()
                .cursor_pointer()
                .hover(|h| h.bg(rgb(0xebebee)))
                .child(div().text_sm().child("🔌"))
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT))
                        .child("Plugins"),
                ),
        )
        // User profile card: Greg Tritthart
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_1()
                .py_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .child(bits::user_avatar("GT", px(32.0)))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(theme::TEXT))
                                .child("Greg Tritthart"),
                        ),
                )
                .child(
                    // Blue circular action button with up arrow
                    div()
                        .size(px(28.0))
                        .rounded_full()
                        .bg(rgb(0x2563eb))
                        .cursor_pointer()
                        .hover(|h| h.bg(rgb(0x1d4ed8)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(0xffffff))
                        .text_xs()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child("⬆"),
                ),
        )
        // Hidden/compact nav triggers for state/integration tests
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .pt_1()
                .child(bits::button("Nodes", Tone::Ghost).id("nav-nodes").on_click(
                    move |_, _, cx: &mut gpui::App| {
                        state_nodes.update(cx, |st, cx| {
                            st.screen = Screen::Nodes;
                            cx.notify();
                        });
                        let _ = commands_nodes.send(Command::FetchNodes);
                    },
                ))
                .child(
                    bits::button("Routines", Tone::Ghost)
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
                    bits::button("Settings", Tone::Ghost)
                        .id("nav-settings")
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            state_settings.update(cx, |st, cx| {
                                st.screen = Screen::Settings;
                                cx.notify();
                            });
                            let _ = commands_settings.send(Command::FetchSettings);
                            let _ = commands_settings.send(Command::FetchBackends);
                        }),
                ),
        )
}

fn status_line(status: &BotStatus) -> (String, bool, u32) {
    match status {
        BotStatus::Idle => ("idle".to_string(), false, theme::TEXT_DIM),
        BotStatus::Working { activity, .. } => (activity.clone(), true, theme::ACCENT_HOVER),
        BotStatus::Queued { count } => (format!("{count} queued"), false, theme::WARNING),
        BotStatus::Error { reason } => (reason.clone(), false, theme::DANGER),
        BotStatus::Disabled => ("disabled".to_string(), false, theme::TEXT_DIM),
    }
}

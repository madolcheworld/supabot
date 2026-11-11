//! The left sidebar: Search bar, bot conversations list, unread indicators,
//! plugins, and user profile footer matching the design.

use std::sync::mpsc::Sender;

use gpui::{Entity, SharedString, div, prelude::*, px, rgb};
use swarmbot_contracts::BotStatus;

use crate::proto::Command;
use crate::state::{AppState, Screen};
use crate::theme;
use crate::widgets::bits::{self, Tone};

pub fn render(s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    let has_unreads = s.unread.values().any(|&u| u > 0);
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
                .child(bots_list(s, state, commands))
                .when(has_unreads, |d| d.child(more_unreads_pill())),
        )
        // Footer: Plugins + User Profile + Quick navigation
        .child(footer(s, state, commands))
}

fn header_search(state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    let state_group = state.clone();
    let commands_h = commands.clone();
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
            // Plus button for adding a bot or creating a group
            div()
                .id("nav-new-group")
                .size(px(36.0))
                .rounded_xl()
                .bg(rgb(0xebebee))
                .cursor_pointer()
                .hover(|h| h.bg(rgb(0xdcdce0)))
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(theme::TEXT))
                .text_lg()
                .font_weight(gpui::FontWeight::MEDIUM)
                .child("+")
                .on_click(move |_, _, cx: &mut gpui::App| {
                    state_group.update(cx, |st, cx| {
                        st.create_group_title = "Team Chat".to_string();
                        st.create_group_members = st
                            .bots
                            .iter()
                            .take(swarmbot_contracts::MAX_GROUP_MEMBERS)
                            .map(|b| b.id.clone())
                            .collect();
                        if st.bots.len() < swarmbot_contracts::MIN_GROUP_MEMBERS {
                            st.create_bot_open = true;
                            st.create_group_open = false;
                        } else {
                            st.create_group_open = true;
                            st.create_bot_open = false;
                        }
                        cx.notify();
                    });
                    let _ = commands_h.send(Command::FetchBackends);
                }),
        )
}

fn bots_list(s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    let mut list = div().flex().flex_col().gap_1();

    // 1. Group Channels Section
    let groups: Vec<&swarmbot_contracts::Conversation> = s
        .conversations
        .iter()
        .filter(|c| c.kind.is_group())
        .collect();
    if !groups.is_empty() {
        list = list.child(
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .text_xs()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::TEXT_DIM))
                .child("GROUPS"),
        );
        for group in &groups {
            let title = s.conversation_title(group);
            let conv_id = group.id.clone();
            let selected = s.open_conversation.as_ref() == Some(&conv_id);
            let preview = s
                .preview(&conv_id)
                .unwrap_or_else(|| "No messages yet".to_string());
            let unread = s.unread(&conv_id);
            let state_h = state.clone();
            let conv_c = conv_id.clone();

            let state_del = state.clone();
            let conv_del = conv_id.clone();
            let title_del = title.clone();

            list = list.child(bot_item(
                &format!("group-{}", group.id),
                &title,
                theme::hex("#8b5cf6"),
                '#',
                "",
                &preview,
                unread > 0,
                selected,
                false,
                0,
                move |cx| {
                    let conv = conv_c.clone();
                    state_h.update(cx, |st, cx| {
                        st.open(conv);
                        cx.notify();
                    });
                },
                Some(Box::new(move |cx: &mut gpui::App| {
                    let conv = conv_del.clone();
                    let t = title_del.clone();
                    state_del.update(cx, |st, cx| {
                        st.confirm_delete =
                            Some(crate::state::DeleteTarget::Group { id: conv, title: t });
                        cx.notify();
                    });
                })),
            ));
        }
    }

    // 2. Direct Messages Section
    if !s.bots.is_empty() {
        list = list.child(
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .text_xs()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::TEXT_DIM))
                .child("DIRECT MESSAGES"),
        );
        for (i, bot) in s.bots.iter().enumerate() {
            let (status_text, working, _color) = status_line(&bot.status);
            let dm = bot.direct_conversation_id.clone();
            let selected = s.open_conversation.as_ref() == Some(&dm)
                || (s.open_conversation.is_none() && groups.is_empty() && i == 0);
            let state_h = state.clone();
            let state_del = state.clone();
            let dm_c = dm.clone();
            let preview = s.preview(&dm).unwrap_or(status_text);
            let unread = s.unread(&dm);
            let bot_id = bot.id.clone();
            let bot_label = bot.label.clone();

            list = list.child(bot_item(
                &format!("bot-{}", bot.id),
                &bot.label,
                theme::bot_color(bot),
                bits::initial(&bot.label),
                "",
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
                Some(Box::new(move |cx: &mut gpui::App| {
                    let bid = bot_id.clone();
                    let blab = bot_label.clone();
                    state_del.update(cx, |st, cx| {
                        st.confirm_delete = Some(crate::state::DeleteTarget::Bot {
                            id: bid,
                            label: blab,
                        });
                        cx.notify();
                    });
                })),
            ));
        }
    }

    if groups.is_empty() && s.bots.is_empty() {
        let state_add = state.clone();
        let commands_add = commands.clone();
        list = list.child(
            div()
                .p_4()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("No bots or groups yet"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .text_center()
                        .child("Click '+' above to add a bot or create a group."),
                )
                .child(
                    div()
                        .id("sidebar-empty-add-bot")
                        .mt_1()
                        .px_3()
                        .py_1p5()
                        .rounded_lg()
                        .bg(rgb(theme::ACCENT))
                        .hover(|h| h.bg(rgb(theme::ACCENT_HOVER)))
                        .cursor_pointer()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(0xffffff))
                        .child("+ Add Bot")
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            state_add.update(cx, |st, cx| {
                                st.create_bot_open = true;
                                st.create_group_open = false;
                                cx.notify();
                            });
                            let _ = commands_add.send(Command::FetchBackends);
                        }),
                ),
        );
    }

    list
}

type DeleteCallback = Box<dyn Fn(&mut gpui::App) + 'static>;

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
    on_delete: Option<DeleteCallback>,
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
                // Row 1: Name + Time + Delete button
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
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .when(!time.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_DIM))
                                            .child(time.to_string()),
                                    )
                                })
                                .when_some(on_delete, |d, on_del| {
                                    d.child(
                                        div()
                                            .id(SharedString::from(format!("del-{}", id)))
                                            .cursor_pointer()
                                            .px_1()
                                            .rounded_md()
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_DIM))
                                            .hover(|h| {
                                                h.text_color(rgb(theme::DANGER)).bg(rgb(0xfee2e2))
                                            })
                                            .child("✕")
                                            .on_click(move |_, _, cx: &mut gpui::App| {
                                                cx.stop_propagation();
                                                on_del(cx);
                                            }),
                                    )
                                }),
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

//! The right side panel: Bot screen monitor, top window controls,
//! and the Routines list matching the screenshot.

use std::sync::mpsc::Sender;

use gpui::{Entity, div, prelude::*, px, rgb};

use crate::proto::Command;
use crate::state::{AppState, Screen};
use crate::theme;
use crate::widgets::bits;
use crate::widgets::text_input::TextInput;

pub fn render(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
    _routine_name: &Entity<TextInput>,
    _routine_prompt: &Entity<TextInput>,
) -> gpui::Div {
    let state_toggle = state.clone();

    // Determine target bot name
    let has_bots = !s.bots.is_empty();
    let bot_name = if let Some(conv) = s.open_conversation() {
        match &conv.kind {
            swarmbot_contracts::ConversationKind::Direct { bot_id } => s
                .bot(bot_id)
                .map(|b| b.label.clone())
                .unwrap_or_else(|| "Bot".to_string()),
            _ => "Group".to_string(),
        }
    } else if let Some(first_bot) = s.bots.first() {
        first_bot.label.clone()
    } else {
        "Bot".to_string()
    };

    div()
        .w(theme::space::GRAPH_W)
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(0xffffff))
        .border_l_1()
        .border_color(rgb(theme::BORDER))
        // 1. Top action bar: toggle sidebar button
        .child(
            div()
                .h(theme::space::HEADER_H)
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .px_4()
                .child(
                    div()
                        .id("collapse-btn")
                        .cursor_pointer()
                        .p_1()
                        .rounded_md()
                        .hover(|h| h.bg(rgb(theme::SURFACE_RAISED)))
                        .child(bits::sidebar_icon(true))
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            state_toggle.update(cx, |st, cx| {
                                st.graph_open = false;
                                cx.notify();
                            });
                        }),
                ),
        )
        // 2. Scrollable content: Team Bots Section + optional Bot Screen Card + Routines Section
        .child({
            let mut scroll = div()
                .id("side-panel-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_4()
                .px_4()
                .py_2()
                // Team Bots Section
                .child(team_bots_section(s, state, commands));
            if has_bots {
                // Top Card: "<BotName>'s screen"
                scroll = scroll.child(screen_card(&bot_name, true));
            }
            // Bottom Section: Routines
            scroll.child(routines_section(s, state, commands))
        })
}

fn screen_card(bot_name: &str, has_bots: bool) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            // Gray display monitor card
            div()
                .h(px(170.0))
                .w_full()
                .rounded_2xl()
                .bg(rgb(0xf0f1f4))
                .flex()
                .items_center()
                .justify_center()
                .child(bits::monitor_icon()),
        )
        // Caption
        .child(
            div()
                .w_full()
                .flex()
                .justify_center()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(if has_bots {
                    format!("{bot_name}'s screen")
                } else {
                    "No active bot screen".to_string()
                }),
        )
}

fn team_bots_section(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
) -> gpui::Div {
    let state_h = state.clone();
    let commands_h = commands.clone();

    let mut section = div()
        .flex()
        .flex_col()
        .gap_2()
        // Header row
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .pt_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(theme::TEXT))
                                .child("Team Bots"),
                        )
                        .when(!s.bots.is_empty(), |d| {
                            d.child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(rgb(theme::SURFACE_RAISED))
                                    .text_xs()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(s.bots.len().to_string()),
                            )
                        }),
                )
                .child(
                    div()
                        .id("add-bot-button")
                        .size(px(28.0))
                        .rounded_lg()
                        .bg(rgb(0xebebee))
                        .cursor_pointer()
                        .hover(|h| h.bg(rgb(0xdcdce0)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(theme::TEXT))
                        .text_base()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child("+")
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            state_h.update(cx, |st, cx| {
                                st.create_bot_open = true;
                                st.create_group_open = false;
                                cx.notify();
                            });
                            let _ = commands_h.send(Command::FetchBackends);
                        }),
                ),
        );

    if !s.bots.is_empty() {
        let mut list = div().flex().flex_col().gap_1p5();
        for bot in &s.bots {
            let state_open = state.clone();
            let state_del = state.clone();
            let bot_id = bot.id.clone();
            let bot_label = bot.label.clone();
            let dm = bot.direct_conversation_id.clone();
            let bot_color = theme::bot_color(bot);
            let bot_initial = bits::initial(&bot.label);
            let bot_handle = bot.name.clone();

            list = list.child(
                div()
                    .id(gpui::SharedString::from(format!("side-bot-{}", bot.id)))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1p5()
                    .rounded_xl()
                    .cursor_pointer()
                    .hover(|h| h.bg(rgb(theme::SURFACE_RAISED)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .flex_1()
                            .child(bits::avatar(bot_color, bot_initial, px(28.0)))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(rgb(theme::TEXT))
                                            .truncate()
                                            .child(bot_label.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .truncate()
                                            .child(format!("@{bot_handle}")),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id(gpui::SharedString::from(format!("side-del-{}", bot.id)))
                            .cursor_pointer()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_DIM))
                            .hover(|h| h.text_color(rgb(theme::DANGER)).bg(rgb(0xfee2e2)))
                            .child("✕")
                            .on_click(move |_, _, cx: &mut gpui::App| {
                                cx.stop_propagation();
                                let bid = bot_id.clone();
                                let blab = bot_label.clone();
                                state_del.update(cx, |st, cx| {
                                    st.confirm_delete = Some(crate::state::DeleteTarget::Bot {
                                        id: bid,
                                        label: blab,
                                    });
                                    cx.notify();
                                });
                            }),
                    )
                    .on_click(move |_, _, cx: &mut gpui::App| {
                        let dm = dm.clone();
                        state_open.update(cx, |st, cx| {
                            st.open(dm);
                            cx.notify();
                        });
                    }),
            );
        }
        section = section.child(list);
    } else {
        let state_empty = state.clone();
        let commands_empty = commands.clone();
        section = section.child(
            div()
                .py_3()
                .px_3()
                .rounded_xl()
                .bg(rgb(theme::SURFACE_RAISED))
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("No bots yet"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child("Add a bot to start chatting or form a group."),
                )
                .child(
                    div()
                        .id("side-empty-add-bot")
                        .px_3()
                        .py_1p5()
                        .rounded_lg()
                        .bg(rgb(theme::ACCENT))
                        .hover(|h| h.bg(rgb(theme::ACCENT_HOVER)))
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::BACKGROUND))
                        .child("+ Add Bot")
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            state_empty.update(cx, |st, cx| {
                                st.create_bot_open = true;
                                st.create_group_open = false;
                                cx.notify();
                            });
                            let _ = commands_empty.send(Command::FetchBackends);
                        }),
                ),
        );
    }

    section
}

fn routines_section(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
) -> gpui::Div {
    let state_h = state.clone();
    let commands_h = commands.clone();

    let mut section = div()
        .flex()
        .flex_col()
        .gap_3()
        // Header row
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .pt_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(theme::TEXT))
                        .child("Routines"),
                )
                .child(
                    div()
                        .id("add-routine-button")
                        .size(px(24.0))
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|h| h.bg(rgb(theme::SURFACE_RAISED)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .text_base()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child("+")
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            state_h.update(cx, |st, cx| {
                                st.screen = Screen::Routines;
                                st.routine_creator_open = true;
                                cx.notify();
                            });
                            let _ = commands_h.send(Command::FetchRoutines);
                        }),
                ),
        );

    let mut list = div().flex().flex_col().gap_3p5();

    if !s.routines.is_empty() {
        for routine in &s.routines {
            let sched = format_schedule(&routine.schedule);
            list = list.child(routine_item(&routine.name, &sched));
        }
    } else {
        list = list.child(
            div()
                .py_2()
                .px_3()
                .rounded_xl()
                .bg(rgb(theme::SURFACE_RAISED))
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .child("No routines yet"),
        );
    }

    section = section.child(list);
    section
}

fn routine_item(name: &str, sched: &str) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap_2p5()
        .child(div().pt(px(2.0)).child(bits::routine_icon()))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT))
                        .truncate()
                        .child(name.to_string()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(sched.to_string()),
                ),
        )
}

fn format_schedule(schedule: &swarmbot_contracts::RoutineSchedule) -> String {
    match schedule {
        swarmbot_contracts::RoutineSchedule::Interval { every_secs } => {
            if *every_secs < 60 {
                format!("Every {every_secs} seconds")
            } else if *every_secs < 3600 {
                format!("Every {} minutes", every_secs / 60)
            } else if *every_secs == 86400 {
                "Every day at 9:00 AM".to_string()
            } else {
                format!("Every {} hours", every_secs / 3600)
            }
        }
        swarmbot_contracts::RoutineSchedule::Cron { expr } => {
            format!("Cron: {expr}")
        }
        swarmbot_contracts::RoutineSchedule::Once { run_at } => {
            format!("Once at {}", run_at.format("%B %-d at %-I:%M %p"))
        }
    }
}

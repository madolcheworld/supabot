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
    let bot_name = if let Some(conv) = s.open_conversation() {
        match &conv.kind {
            swarmbot_contracts::ConversationKind::Direct { bot_id } => s
                .bot(bot_id)
                .map(|b| b.label.clone())
                .unwrap_or_else(|| "Waypoint".to_string()),
            _ => "Waypoint".to_string(),
        }
    } else {
        "Waypoint".to_string()
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
        // 2. Scrollable content: Bot Screen Card + Routines Section
        .child(
            div()
                .id("side-panel-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_4()
                .px_4()
                .py_2()
                // Top Card: "<BotName>'s screen"
                .child(screen_card(&bot_name))
                // Bottom Section: Routines
                .child(routines_section(s, state, commands)),
        )
}

fn screen_card(bot_name: &str) -> gpui::Div {
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
                .child(format!("{bot_name}'s screen")),
        )
}

fn routines_section(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
) -> gpui::Div {
    let state_h = state.clone();
    let commands_h = commands.clone();

    // Default sample routines from the screenshot
    let default_routines = [
        ("Check Waypoint inbox", "Every day at 10:27 AM"),
        ("Liesl food surprise reminder", "Every Thursday at 5:00 PM"),
        (
            "Black Oak Halloween window check",
            "Every October 1 at 9:00 AM",
        ),
        ("Basecamp day-of email", "Every November 20 at 8:00 AM"),
        (
            "Black Oak 2027 first-weekend open",
            "Every November 21 at 8:00 AM",
        ),
        (
            "Black Oak 2027 Halloween weekend open",
            "Every November 28 at 8:00 AM",
        ),
        (
            "February trip planning check",
            "Every February 1 at 9:00 AM",
        ),
    ];

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

    // Render real routines if configured
    if !s.routines.is_empty() {
        for routine in &s.routines {
            let sched = format_schedule(&routine.schedule);
            list = list.child(routine_item(&routine.name, &sched));
        }
    } else {
        // Render the exact routines list from the screenshot
        for (name, sched) in default_routines {
            list = list.child(routine_item(name, sched));
        }
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

//! The right side panel: Bot screen monitor, top window controls,
//! and the Routines list matching the screenshot.

use std::sync::mpsc::Sender;

use chrono::{DateTime, FixedOffset, Local};
use gpui::{Entity, div, prelude::*, px, rgb};

use crate::proto::Command;
use crate::state::{AppState, Fetched, Screen};
use crate::theme;
use crate::view::graph;
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

    // Determine target bot name. The bot screen card is only shown when there is
    // a bot to describe; an unknown roster has nothing to draw.
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
        // 2. Scrollable content: optional Bot Screen Card + Routines Section
        .child({
            let mut scroll = div()
                .id("side-panel-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_4()
                .px_4()
                .py_2();
            if has_bots {
                // Top Card: "<BotName>'s screen"
                scroll = scroll.child(screen_card(&bot_name));
            }
            // Middle Section: Delegation tree & selected task steps.
            scroll = scroll.child(graph::render_section(s, state, commands));
            // Bottom Section: Routines
            scroll.child(routines_section(s, state, commands))
        })
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

    // Three states again: this panel is small, so it states the fact in one line
    // and leaves the retry to the Routines screen rather than growing a button.
    match &s.routines {
        Fetched::Ready(routines) if routines.is_empty() => {
            list = list.child(dim_note("No routines scheduled."));
        }
        Fetched::Ready(routines) => {
            for routine in routines {
                let sched = format_schedule(&routine.schedule);
                list = list.child(routine_item(&routine.name, &sched));
            }
        }
        Fetched::Unknown => {
            list = list.child(dim_note("Routines not loaded yet."));
        }
        Fetched::Failed(reason) => {
            list = list.child(dim_note(&format!("Routines could not be loaded: {reason}")));
        }
    }

    section = section.child(list);
    section
}

fn dim_note(text: &str) -> gpui::Div {
    div()
        .py_2()
        .px_3()
        .rounded_xl()
        .bg(rgb(theme::SURFACE_RAISED))
        .text_xs()
        .text_color(rgb(theme::TEXT_DIM))
        .child(text.to_string())
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
    // `Local::now()` is read here and nowhere else, so the labelled function
    // below stays pure and a test can pin both the zone and the wording.
    format_schedule_at(schedule, Local::now().fixed_offset())
}

/// [`format_schedule`] against a supplied clock. Pure, so its assertions do not
/// depend on the machine's time zone or on the day the suite runs.
///
/// A `Once` run time is rendered through [`crate::time::moment_label`], the same
/// day-aware formatter the transcript uses. It used to be printed as
/// `run_at.format("%B %-d at %-I:%M %p")` on a **UTC** value: "November 14 at
/// 2:00 AM" for an instant the owner's clock calls 9:00 AM, with no year at all
/// for a run a year out — two clocks in one app, and the wrong one here, since
/// this line answers "when will it run?".
fn format_schedule_at(
    schedule: &swarmbot_contracts::RoutineSchedule,
    now: DateTime<FixedOffset>,
) -> String {
    match schedule {
        swarmbot_contracts::RoutineSchedule::Interval { every_secs } => {
            if *every_secs < 60 {
                format!("Every {every_secs} seconds")
            } else if *every_secs < 3600 {
                format!("Every {} minutes", every_secs / 60)
            } else {
                // A 86400-second interval used to print "Every day at 9:00 AM".
                // An interval has no time of day — it fires 24 hours after the
                // previous run, whenever that was — so naming 9:00 AM was a
                // claim the schedule does not make.
                format!("Every {} hours", every_secs / 3600)
            }
        }
        swarmbot_contracts::RoutineSchedule::Cron { expr } => {
            format!("Cron: {expr}")
        }
        swarmbot_contracts::RoutineSchedule::Once { run_at } => {
            format!("Once, {}", crate::time::moment_label(now, *run_at))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use swarmbot_contracts::RoutineSchedule;

    /// A fixed +07:00 zone, written out so the assertions do not depend on the
    /// machine's `TZ`.
    fn zone() -> FixedOffset {
        FixedOffset::east_opt(7 * 3600).expect("+07:00 is a valid offset")
    }

    fn now(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<FixedOffset> {
        zone()
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("a real local time")
    }

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("a real UTC time")
    }

    /// A `Once` run time is stated in the viewer's zone and names its day. It used
    /// to be printed straight off the UTC value (`%B %-d at %-I:%M %p`), so an
    /// instant the owner's clock calls 9:00 AM read "2:00 AM", and a run in
    /// another year carried no year at all.
    #[test]
    fn a_once_schedule_is_stated_in_the_viewers_zone_and_names_its_day() {
        let local_noon = now(2026, 11, 14, 12, 0);
        let schedule = |run_at| RoutineSchedule::Once { run_at };
        assert_eq!(
            format_schedule_at(&schedule(utc(2026, 11, 14, 2, 0)), local_noon),
            "Once, Today 9:00 AM"
        );
        assert_eq!(
            format_schedule_at(&schedule(utc(2026, 11, 13, 2, 0)), local_noon),
            "Once, Yesterday 9:00 AM"
        );
        assert_eq!(
            format_schedule_at(&schedule(utc(2026, 11, 3, 2, 0)), local_noon),
            "Once, Nov 3, 9:00 AM"
        );
        // Another year keeps its year, which the old format dropped entirely.
        assert_eq!(
            format_schedule_at(&schedule(utc(2025, 11, 3, 2, 0)), local_noon),
            "Once, Nov 3, 2025, 9:00 AM"
        );
    }

    /// An interval has no time of day: 86400 seconds means "24 hours after the
    /// previous run", not "at 9:00 AM". The card said the latter.
    #[test]
    fn an_interval_states_a_period_rather_than_a_clock_time() {
        let text = format_schedule_at(
            &RoutineSchedule::Interval { every_secs: 86400 },
            now(2026, 11, 14, 12, 0),
        );
        assert_eq!(text, "Every 24 hours");
        assert!(
            !text.contains("AM") && !text.contains("PM"),
            "an interval has no clock time, so it must not print one: {text}"
        );
        // The shorter units keep their wording.
        assert_eq!(
            format_schedule_at(
                &RoutineSchedule::Interval { every_secs: 30 },
                now(2026, 11, 14, 12, 0)
            ),
            "Every 30 seconds"
        );
        assert_eq!(
            format_schedule_at(
                &RoutineSchedule::Interval { every_secs: 600 },
                now(2026, 11, 14, 12, 0)
            ),
            "Every 10 minutes"
        );
    }
}

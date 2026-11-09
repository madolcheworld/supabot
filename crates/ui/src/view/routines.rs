//! The Routines screen: scheduled background triggers that wake bots.
//!
//! A routine is an instruction or prompt that runs on an interval, at a fixed
//! time, or on a cron schedule. Operators can view active/paused routines,
//! inspect recent execution runs, pause/resume, delete, or create a new routine.

use std::sync::mpsc::Sender;

use gpui::{App, Entity, SharedString, div, prelude::*, rgb};
use swarmbot_client::NewRoutineRequest;
use swarmbot_contracts::{
    Routine, RoutineId, RoutineRun, RoutineSchedule, RoutineStatus, RunStatus,
};

use crate::proto::Command;
use crate::state::AppState;
use crate::theme;
use crate::widgets::bits::{self, Tone};
use crate::widgets::text_input::TextInput;

pub fn render(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
    name_input: &Entity<TextInput>,
    prompt_input: &Entity<TextInput>,
) -> gpui::Div {
    let commands_fetch = commands.clone();
    let state_toggle = state.clone();

    div()
        .size_full()
        .flex()
        .flex_col()
        .child(
            div()
                .px_4()
                .py_2()
                .flex()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(rgb(theme::BORDER))
                .child(
                    div()
                        .text_lg()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(theme::TEXT))
                        .child("Routines"),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            bits::button(
                                if s.routine_creator_open {
                                    "Close creator"
                                } else {
                                    "New Routine"
                                },
                                if s.routine_creator_open {
                                    Tone::Ghost
                                } else {
                                    Tone::Primary
                                },
                            )
                            .id("toggle-routine-creator")
                            .on_click(move |_, _, cx: &mut App| {
                                state_toggle.update(cx, |st, cx| {
                                    st.routine_creator_open = !st.routine_creator_open;
                                    cx.notify();
                                });
                            }),
                        )
                        .child(
                            bits::button("Refresh", Tone::Ghost)
                                .id("refresh-routines")
                                .on_click(move |_, _, _: &mut App| {
                                    let _ = commands_fetch.send(Command::FetchRoutines);
                                }),
                        ),
                ),
        )
        .child(
            div()
                .id("routines-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_4()
                .px_4()
                .py_3()
                .when(s.routine_creator_open, |d| {
                    d.child(creator_section(
                        s,
                        state,
                        commands,
                        name_input,
                        prompt_input,
                    ))
                })
                .child(routines_list(s, state, commands)),
        )
}

fn creator_section(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
    name_input: &Entity<TextInput>,
    prompt_input: &Entity<TextInput>,
) -> gpui::Div {
    let state_bot = state.clone();
    let state_sched = state.clone();
    let state_cancel = state.clone();
    let commands_create = commands.clone();
    let name_h = name_input.clone();
    let prompt_h = prompt_input.clone();
    // Default target bot to first bot in roster if not set
    let active_bot_id = s
        .routine_draft_bot
        .clone()
        .or_else(|| s.bots.first().map(|b| b.id.clone()));
    let draft_bot = active_bot_id.clone();
    let draft_sched = s.routine_draft_schedule.clone();

    let presets: [(&str, RoutineSchedule); 6] = [
        ("Every 1 min", RoutineSchedule::Interval { every_secs: 60 }),
        (
            "Every 5 mins",
            RoutineSchedule::Interval { every_secs: 300 },
        ),
        (
            "Every 1 hour",
            RoutineSchedule::Interval { every_secs: 3600 },
        ),
        (
            "Every 24 hours",
            RoutineSchedule::Interval { every_secs: 86400 },
        ),
        (
            "Hourly cron (0 * * * *)",
            RoutineSchedule::Cron {
                expr: "0 * * * *".to_string(),
            },
        ),
        (
            "Weekdays 9am (0 9 * * 1-5)",
            RoutineSchedule::Cron {
                expr: "0 9 * * 1-5".to_string(),
            },
        ),
    ];

    div()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::ACCENT))
        .bg(rgb(theme::SURFACE))
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .child(bits::section_label("SCHEDULE A NEW ROUTINE"))
        // Target Bot selection
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Target Bot:"),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .children(s.bots.iter().map(|bot| {
                            let bot_id = bot.id.clone();
                            let is_active = active_bot_id.as_ref() == Some(&bot.id);
                            let state_b = state_bot.clone();
                            let b = bits::button(
                                &bot.label,
                                if is_active {
                                    Tone::Primary
                                } else {
                                    Tone::Ghost
                                },
                            );
                            b.id(SharedString::from(format!("draft-bot-{}", bot.id)))
                                .on_click(move |_, _, cx: &mut App| {
                                    let id = bot_id.clone();
                                    state_b.update(cx, |st, cx| {
                                        st.routine_draft_bot = Some(id);
                                        cx.notify();
                                    });
                                })
                        })),
                ),
        )
        // Name input
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Name:"),
                )
                .child(name_input.clone()),
        )
        // Prompt input
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Instruction / Prompt:"),
                )
                .child(prompt_input.clone()),
        )
        // Schedule Presets
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Schedule:"),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .children(
                            presets
                                .into_iter()
                                .enumerate()
                                .map(|(idx, (label, sched))| {
                                    let is_match = s.routine_draft_schedule == sched;
                                    let state_s = state_sched.clone();
                                    let b = bits::button(
                                        label,
                                        if is_match { Tone::Primary } else { Tone::Ghost },
                                    );
                                    b.id(SharedString::from(format!("draft-sched-{}", idx)))
                                        .on_click(move |_, _, cx: &mut App| {
                                            let sched = sched.clone();
                                            state_s.update(cx, |st, cx| {
                                                st.routine_draft_schedule = sched;
                                                cx.notify();
                                            });
                                        })
                                }),
                        ),
                ),
        )
        // Action buttons
        .child(
            div()
                .flex()
                .justify_end()
                .gap_2()
                .pt_2()
                .child(
                    bits::button("Cancel", Tone::Ghost)
                        .id("cancel-create-routine")
                        .on_click(move |_, _, cx: &mut App| {
                            state_cancel.update(cx, |st, cx| {
                                st.routine_creator_open = false;
                                cx.notify();
                            });
                        }),
                )
                .child(
                    bits::button("Schedule Routine", Tone::Primary)
                        .id("submit-create-routine")
                        .on_click(move |_, _, cx: &mut App| {
                            let name = name_h.read(cx).content().to_string();
                            let prompt = prompt_h.read(cx).content().to_string();
                            if name.trim().is_empty() {
                                return;
                            }
                            let bot_handle = if let Some(target) = draft_bot.as_ref() {
                                target.as_str().to_string()
                            } else {
                                return;
                            };
                            let req = NewRoutineRequest::new(
                                bot_handle,
                                name,
                                prompt,
                                draft_sched.clone(),
                            );
                            let _ = commands_create.send(Command::CreateRoutine(req));
                        }),
                ),
        )
}

fn routines_list(s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    if s.routines.is_empty() {
        return div()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .p_6()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgb(theme::TEXT))
                    .child("No routines scheduled"),
            )
            .child(div().text_sm().text_color(rgb(theme::TEXT_MUTED)).child(
                "Routines run prompt deliveries on an interval, one-time, or cron schedule.",
            ));
    }

    let mut list = div().flex().flex_col().gap_3();
    for routine in &s.routines {
        list = list.child(routine_card(routine, s, state, commands));
    }
    list
}

fn routine_card(
    routine: &Routine,
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
) -> gpui::Div {
    let id_pause = routine.id.clone();
    let id_resume = routine.id.clone();
    let id_delete = routine.id.clone();
    let id_runs = routine.id.clone();
    let id_select = routine.id.clone();

    let commands_pause = commands.clone();
    let commands_resume = commands.clone();
    let commands_delete = commands.clone();
    let commands_runs = commands.clone();
    let state_runs = state.clone();

    let is_selected = s.selected_routine.as_ref() == Some(&routine.id);
    let target_bot = s.bot(&routine.bot_id);
    let bot_label = target_bot
        .map(|b| b.label.as_str())
        .unwrap_or(routine.bot_id.as_str());

    let (status_label, status_bg, status_fg) = match routine.status {
        RoutineStatus::Active => ("active", theme::SUCCESS, 0x0b1410),
        RoutineStatus::Paused => ("paused", theme::WARNING, 0x0b1410),
        RoutineStatus::Completed => ("completed", theme::SURFACE_RAISED, theme::TEXT_MUTED),
    };

    let next_run_str = routine
        .next_run_at
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "none".to_string());

    let last_run_str = routine
        .last_run_at
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "never".to_string());

    let mut card = div()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(if is_selected {
            rgb(theme::ACCENT)
        } else {
            rgb(theme::BORDER)
        })
        .bg(rgb(theme::SURFACE))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        // Top row: Avatar + Name + Bot + Status badge
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .children(target_bot.map(|b| {
                            bits::avatar(
                                theme::bot_color(b),
                                bits::initial(&b.label),
                                theme::space::AVATAR,
                            )
                        }))
                        .child(
                            div()
                                .text_base()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(theme::TEXT))
                                .child(routine.name.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(format!("for @{}", bot_label)),
                        ),
                )
                .child(bits::badge(status_label, status_bg, status_fg)),
        )
        // Schedule + Runs info
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_4()
                .text_xs()
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(div().text_color(rgb(theme::TEXT_DIM)).child("Schedule:"))
                        .child(
                            div()
                                .text_color(rgb(theme::TEXT))
                                .child(format_schedule(&routine.schedule)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(div().text_color(rgb(theme::TEXT_DIM)).child("Next run:"))
                        .child(div().text_color(rgb(theme::TEXT)).child(next_run_str)),
                )
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(div().text_color(rgb(theme::TEXT_DIM)).child("Last run:"))
                        .child(div().text_color(rgb(theme::TEXT)).child(last_run_str)),
                ),
        )
        // Prompt box
        .child(
            div()
                .w_full()
                .rounded_sm()
                .bg(rgb(theme::CODE_BG))
                .px_2()
                .py_1()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .truncate()
                .child(format!("Prompt: \"{}\"", routine.prompt)),
        )
        // Buttons
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .pt_1()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .when(routine.status == RoutineStatus::Active, |d| {
                            d.child(
                                bits::button("Pause", Tone::Ghost)
                                    .id(SharedString::from(format!("pause-{}", routine.id)))
                                    .on_click(move |_, _, _: &mut App| {
                                        let _ = commands_pause
                                            .send(Command::PauseRoutine(id_pause.clone()));
                                    }),
                            )
                        })
                        .when(routine.status == RoutineStatus::Paused, |d| {
                            d.child(
                                bits::button("Resume", Tone::Ghost)
                                    .id(SharedString::from(format!("resume-{}", routine.id)))
                                    .on_click(move |_, _, _: &mut App| {
                                        let _ = commands_resume
                                            .send(Command::ResumeRoutine(id_resume.clone()));
                                    }),
                            )
                        })
                        .child(
                            bits::button(
                                if is_selected { "Hide runs" } else { "Runs" },
                                Tone::Ghost,
                            )
                            .id(SharedString::from(format!("runs-{}", routine.id)))
                            .on_click(move |_, _, cx: &mut App| {
                                let id = id_runs.clone();
                                let sel = id_select.clone();
                                let cmd = commands_runs.clone();
                                state_runs.update(cx, |st, cx| {
                                    if st.selected_routine.as_ref() == Some(&sel) {
                                        st.selected_routine = None;
                                    } else {
                                        st.selected_routine = Some(sel);
                                        let _ = cmd.send(Command::FetchRoutineRuns(id));
                                    }
                                    cx.notify();
                                });
                            }),
                        ),
                )
                .child(
                    bits::button("Delete", Tone::Danger)
                        .id(SharedString::from(format!("delete-{}", routine.id)))
                        .on_click(move |_, _, _: &mut App| {
                            let _ = commands_delete.send(Command::DeleteRoutine(id_delete.clone()));
                        }),
                ),
        );

    // If expanded, show recent runs
    if is_selected {
        card = card.child(runs_subview(&routine.id, s));
    }

    card
}

fn runs_subview(routine_id: &RoutineId, s: &AppState) -> gpui::Div {
    let runs = s.routine_runs.get(routine_id);
    let mut drawer = div()
        .w_full()
        .rounded_md()
        .border_t_1()
        .border_color(rgb(theme::BORDER))
        .pt_2()
        .flex()
        .flex_col()
        .gap_2()
        .child(bits::section_label("RECENT RUNS"));

    match runs {
        None => {
            drawer = drawer.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("loading runs…"),
            );
        }
        Some(list) if list.is_empty() => {
            drawer = drawer.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("no execution runs recorded yet"),
            );
        }
        Some(list) => {
            for run in list {
                drawer = drawer.child(run_row(run));
            }
        }
    }

    drawer
}

fn run_row(run: &RoutineRun) -> gpui::Div {
    let (status_label, bg, fg) = match run.status {
        RunStatus::Running => ("running", theme::ACCENT, 0xffffff),
        RunStatus::Success => ("success", theme::SUCCESS, 0x0b1410),
        RunStatus::Failed => ("failed", theme::DANGER, 0xffffff),
    };

    let started = run.started_at.format("%Y-%m-%d %H:%M:%S UTC").to_string();
    let ended = run
        .ended_at
        .map(|e| e.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "in progress".to_string());

    div()
        .w_full()
        .rounded_sm()
        .bg(rgb(theme::SURFACE_RAISED))
        .p_2()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(bits::badge(status_label, bg, fg))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(theme::TEXT))
                                .child(run.id.as_str().to_string()),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(format!("{started} -> {ended}")),
                ),
        )
        .children(run.error.as_ref().map(|err| {
            div()
                .text_xs()
                .text_color(rgb(theme::DANGER))
                .child(format!("Error: {err}"))
        }))
}

fn format_schedule(schedule: &RoutineSchedule) -> String {
    match schedule {
        RoutineSchedule::Interval { every_secs } => {
            if *every_secs < 60 {
                format!("Every {every_secs}s")
            } else if *every_secs < 3600 {
                format!("Every {}m", every_secs / 60)
            } else if *every_secs < 86400 {
                format!("Every {}h", every_secs / 3600)
            } else {
                format!("Every {}d", every_secs / 86400)
            }
        }
        RoutineSchedule::Once { run_at } => {
            format!("Once at {}", run_at.format("%Y-%m-%d %H:%M UTC"))
        }
        RoutineSchedule::Cron { expr } => format!("Cron: {expr}"),
    }
}

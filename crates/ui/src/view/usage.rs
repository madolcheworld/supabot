//! The Usage screen: what the team spent, **as the backends reported it**.
//!
//! Two rules from [`swarmbot_contracts::turn::UsageReport`] decide what this
//! screen may claim, and both are visible in the UI rather than buried in a
//! doc comment:
//!
//! * **zeroes mean "nothing was reported", not "nothing was spent".** A backend
//!   that reports no usage (an ACP agent, the fake one) makes its turns look
//!   free. The turn count is therefore rendered next to the totals, because
//!   "0 turns" and "3 turns, no numbers" are both zeros and only one of them
//!   means nothing happened.
//! * **no price.** Turning tokens into money needs the owner's per-model rates;
//!   a built-in table would be a guess with a decimal point. The screen says
//!   what it is not instead of inventing a currency.
//!
//! The screen is read-only: nothing here can change the spend, so nothing here
//! writes to the brain.

use std::sync::mpsc::Sender;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use gpui::{App, Entity, div, prelude::*, px, rgb};
use swarmbot_contracts::turn::{BotUsage, UsageReport};

use crate::proto::Command;
use crate::state::AppState;
use crate::theme;
use crate::widgets::bits::{self, Tone};

/// The windows offered, in hours. `24` is the brain's own default, so the first
/// fetch asks exactly what a bare `GET /api/usage` would.
pub const WINDOWS: [(i64, &str); 3] = [(24, "24h"), (168, "7d"), (720, "30d")];

/// The `since` instant for a window, as the RFC 3339 text the route parses.
///
/// Computed from the caller's clock at click time, not from a value captured
/// when the app started: an app left open overnight must not ask "last 24
/// hours" and get the day before. `now` is a parameter so a test can pin it.
pub fn since_for(hours: i64, now: DateTime<Utc>) -> String {
    (now - Duration::hours(hours)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `24 hours`, `7 days` — the window in words, for the header.
pub fn window_label(hours: i64) -> String {
    if hours % 24 == 0 && hours >= 24 {
        format!("last {} day{}", hours / 24, plural(hours / 24))
    } else {
        format!("last {hours} hour{}", plural(hours))
    }
}

fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Group digits so a six-figure token count is readable at a glance.
///
/// Hand-rolled rather than pulled from a crate: the workspace does not carry a
/// number-formatting dependency, and this is the only place that needs one.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        // A group boundary before this digit: three from the end, then every
        // three. `digits.len() - i` is the number of digits still to come
        // including this one, so this is true exactly at the boundaries.
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The honest caption under the numbers. One sentence, always true.
pub fn honesty_note(report: &UsageReport) -> String {
    if report.is_empty() {
        return "No turn started in this window, so there is nothing to report. \
                These are not zero-cost turns — there were no turns."
            .to_string();
    }
    if report.totals.total_tokens() == 0 {
        return format!(
            "{} turn{} ran, but no backend reported token counts — the token rows read zero \
             because of silence, not because the turns were free.",
            report.turns,
            if report.turns == 1 { "" } else { "s" }
        );
    }
    "Every number is what a backend reported on a model round trip. A backend that reports \
     nothing makes its turns look free, so read these against the turn count. No price is \
     shown: that needs your own per-model rates."
        .to_string()
}

/// One bot's row: the label to show and whether its name is still known.
///
/// A deleted bot keeps its row (its spend is real) with the id as the label —
/// dropping the row would understate what the team cost.
pub fn bot_label(row: &BotUsage) -> String {
    match &row.bot_name {
        Some(name) => format!("@{name}"),
        None => format!("{} (deleted)", row.bot_id.as_str()),
    }
}

pub fn render(s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    let commands_refresh = commands.clone();
    let since = s.usage_hours;

    let mut window_row = div().flex().items_center().gap_1();
    for (hours, label) in WINDOWS {
        let state_c = state.clone();
        let cmds = commands.clone();
        let active = hours == s.usage_hours;
        window_row = window_row.child(
            bits::button(label, if active { Tone::Primary } else { Tone::Ghost })
                .id(("usage-window", hours as u64))
                .on_click(move |_, _, cx: &mut App| {
                    state_c.update(cx, |st, cx| {
                        st.usage_hours = hours;
                        cx.notify();
                    });
                    let _ = cmds.send(Command::FetchUsage {
                        since: Some(since_for(hours, Utc::now())),
                    });
                }),
        );
    }

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
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(theme::TEXT))
                                .child("Usage"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(window_label(s.usage_hours)),
                        ),
                )
                .child(
                    div().flex().items_center().gap_2().child(window_row).child(
                        bits::button("Refresh", Tone::Ghost)
                            .id("refresh-usage")
                            .on_click(move |_, _, _: &mut App| {
                                let _ = commands_refresh.send(Command::FetchUsage {
                                    since: Some(since_for(since, Utc::now())),
                                });
                            }),
                    ),
                ),
        )
        .child(
            div()
                .id("usage-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_4()
                .px_4()
                .py_3()
                .child(match &s.usage {
                    None => not_loaded(),
                    Some(Err(reason)) => failed(reason, commands, s.usage_hours),
                    Some(Ok(report)) => report_body(report),
                }),
        )
}

/// Before the first fetch: say so, and offer the fetch. An empty report here
/// would be a claim about the team that nobody asked for.
fn not_loaded() -> gpui::Div {
    div()
        .text_sm()
        .text_color(rgb(theme::TEXT_MUTED))
        .child("Not loaded yet. Pick a window to ask the brain what the team spent.")
}

/// A failed fetch: the reason, verbatim, and a way to try again.
fn failed(reason: &str, commands: &Sender<Command>, hours: i64) -> gpui::Div {
    let cmds = commands.clone();
    div()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::NOTICE_ERROR_BORDER))
        .bg(rgb(theme::NOTICE_ERROR_BG))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::NOTICE_ERROR_FG))
                .child("Could not read the usage report"),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::NOTICE_ERROR_FG))
                .child(reason.to_string()),
        )
        .child(
            div().child(
                bits::button("Retry", Tone::Ghost)
                    .id("retry-usage")
                    .on_click(move |_, _, _: &mut App| {
                        let _ = cmds.send(Command::FetchUsage {
                            since: Some(since_for(hours, Utc::now())),
                        });
                    }),
            ),
        )
}

fn report_body(report: &UsageReport) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_4()
        .child(window_section(report))
        .child(totals_section(report))
        .child(by_bot_section(report))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .child(honesty_note(report)),
        )
}

fn window_section(report: &UsageReport) -> gpui::Div {
    div()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .p_3()
        .flex()
        .flex_col()
        .gap_1()
        .child(bits::section_label("WINDOW"))
        .child(bits::kv(
            "from",
            report.since.to_rfc3339_opts(SecondsFormat::Secs, true),
        ))
        .child(bits::kv(
            "to",
            report.until.to_rfc3339_opts(SecondsFormat::Secs, true),
        ))
        // The denominator behind every zero below.
        .child(bits::kv("turns", report.turns.to_string()))
}

fn totals_section(report: &UsageReport) -> gpui::Div {
    let totals = report.totals;
    div()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .p_3()
        .flex()
        .flex_col()
        .gap_1()
        .child(bits::section_label("TOTALS"))
        .child(bits::kv("input tokens", thousands(totals.input_tokens)))
        .child(bits::kv("output tokens", thousands(totals.output_tokens)))
        .child(bits::kv("model calls", thousands(totals.llm_calls as u64)))
        .child(bits::kv("tool calls", thousands(totals.tool_calls as u64)))
}

fn by_bot_section(report: &UsageReport) -> gpui::Div {
    let mut section = div()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(bits::section_label("BY BOT"));

    if report.by_bot.is_empty() {
        return section.child(
            div()
                .text_sm()
                .text_color(rgb(theme::TEXT_MUTED))
                .child("No bot ran a turn in this window."),
        );
    }

    section = section.child(header_row());
    for row in &report.by_bot {
        let color = theme::bot_color_from_id(row.bot_id.as_str());
        section = section.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(bits::avatar(
                    color,
                    bits::initial(row.bot_name.as_deref().unwrap_or("?")),
                    px(20.0),
                ))
                .child(
                    div()
                        .w(px(220.0))
                        .flex_none()
                        .text_sm()
                        .text_color(rgb(theme::TEXT))
                        .child(bot_label(row)),
                )
                .child(cell(thousands(row.usage.input_tokens)))
                .child(cell(thousands(row.usage.output_tokens)))
                .child(cell(thousands(row.usage.llm_calls as u64)))
                .child(cell(thousands(row.usage.tool_calls as u64))),
        );
    }
    section
}

fn header_row() -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .text_color(rgb(theme::TEXT_DIM))
        // Aligns with the avatar + label columns above.
        .child(div().w(px(28.0)).flex_none())
        .child(div().w(px(220.0)).flex_none().child("bot"))
        .child(heading_cell("in"))
        .child(heading_cell("out"))
        .child(heading_cell("model"))
        .child(heading_cell("tools"))
}

fn heading_cell(label: &str) -> gpui::Div {
    div()
        .w(px(72.0))
        .flex_none()
        .text_xs()
        .text_color(rgb(theme::TEXT_DIM))
        .child(label.to_string())
}

fn cell(value: String) -> gpui::Div {
    div()
        .w(px(72.0))
        .flex_none()
        .text_sm()
        .text_color(rgb(theme::TEXT_MUTED))
        .child(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use swarmbot_contracts::{BotId, Turn};

    fn report(turns: &[Turn]) -> UsageReport {
        UsageReport::from_turns(Utc::now() - Duration::hours(24), Utc::now(), turns, |_| {
            None
        })
    }

    #[test]
    fn a_window_ends_at_the_moment_it_was_asked_from() {
        let now = Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap();
        assert_eq!(since_for(24, now), "2026-11-12T12:00:00Z");
        assert_eq!(since_for(168, now), "2026-11-06T12:00:00Z");
    }

    #[test]
    fn the_window_is_named_in_the_unit_that_reads_best() {
        assert_eq!(window_label(24), "last 1 day");
        assert_eq!(window_label(48), "last 2 days");
        assert_eq!(window_label(168), "last 7 days");
        assert_eq!(window_label(6), "last 6 hours");
        assert_eq!(window_label(1), "last 1 hour");
    }

    #[test]
    fn digit_groups_are_inserted_at_the_right_places() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(12_345), "12,345");
        assert_eq!(thousands(1_288_000), "1,288,000");
    }

    #[test]
    fn an_empty_window_says_no_turn_ran_rather_than_zero_spend() {
        let note = honesty_note(&report(&[]));
        assert!(note.contains("No turn started"), "{note}");
        // It must not report a spend of zero; it must report *no turns*.
        assert!(note.contains("there were no turns"), "{note}");
    }

    #[test]
    fn turns_with_unreported_usage_are_called_silence_not_zero_cost() {
        // A turn with no usage at all is exactly what a backend that reports
        // nothing produces: `turns = 1`, every counter zero.
        let mut turn = Turn::start(
            BotId::from_suffix("lead"),
            swarmbot_contracts::ConversationId::from_suffix("dm-lead"),
            swarmbot_contracts::TurnInitiator::Owner,
            None,
            None,
        );
        turn.usage = Default::default();
        let note = honesty_note(&report(&[turn]));
        assert!(note.contains("1 turn ran"), "{note}");
        assert!(note.contains("silence"), "{note}");
    }

    #[test]
    fn a_deleted_bot_keeps_its_row_and_says_it_is_gone() {
        let row = BotUsage {
            bot_id: BotId::from_suffix("gone"),
            bot_name: None,
            usage: Default::default(),
        };
        let label = bot_label(&row);
        assert!(label.contains("deleted"), "{label}");
        assert!(label.contains("gone"), "{label}");

        let named = BotUsage {
            bot_id: BotId::from_suffix("lead"),
            bot_name: Some("lead".into()),
            usage: Default::default(),
        };
        assert_eq!(bot_label(&named), "@lead");
    }
}

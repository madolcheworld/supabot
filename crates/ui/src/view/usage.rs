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
use crate::view::settings::ghost_button;
use crate::widgets::bits::{self, Tone};

/// The windows offered, in hours. `24` is the brain's own default, so the first
/// fetch asks exactly what a bare `GET /api/usage` would.
pub const WINDOWS: [(i64, &str); 3] = [(24, "24h"), (168, "7d"), (720, "30d")];

/// Width of the bot-name cell in a BY BOT row, and in the header above it.
///
/// The cell **truncates** (`min_w_0().truncate()`): it used to be a fixed
/// `w(220px).flex_none()` with no ellipsis, so a long `@name` ran straight over
/// the first numeric column and the two texts drew on top of each other.
pub const BOT_LABEL_W: f32 = 220.0;

/// Width of one numeric column in a BY BOT row.
///
/// [`thousands`] can emit `12,345,678`, which is ~76px at the row's 14px size —
/// so the old fixed 72px column was narrower than a value it has to show and a
/// seven-figure count spilled over the column edge. 88px holds nine digits with
/// separators, and every row (and the header) uses this one width, right-aligned,
/// so the digits line up instead of jittering left-to-right by magnitude.
pub const NUMERIC_COL_W: f32 = 88.0;

/// Rough advance width of a numeric cell: ~0.6em per digit and ~0.3em per
/// thousands separator, in the row's 14px size.
///
/// An estimate, not a measurement — the real advance needs a text system, and
/// the column only has to be wide enough rather than exact. It is a pure
/// function so [`tests::a_numeric_column_is_wide_enough_for_what_it_shows`] can
/// pin the column against the widest value the screen can produce.
pub fn numeric_text_width(value: &str) -> f32 {
    const EM: f32 = 14.0;
    value
        .chars()
        .map(|ch| if ch == ',' { 0.3 * EM } else { 0.6 * EM })
        .sum()
}

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
        // The inactive windows are `Ghost` buttons on white; the ringed variant,
        // or the range control has no visible button face at all.
        let button = if active {
            bits::button(label, Tone::Primary)
        } else {
            ghost_button(label)
        };
        window_row = window_row.child(button.id(("usage-window", hours as u64)).on_click(
            move |_, _, cx: &mut App| {
                state_c.update(cx, |st, cx| {
                    st.usage_hours = hours;
                    cx.notify();
                });
                let _ = cmds.send(Command::FetchUsage {
                    since: Some(since_for(hours, Utc::now())),
                });
            },
        ));
    }

    let window_caption = window_label(s.usage_hours);
    div()
        .size_full()
        .flex()
        .flex_col()
        // One title row for every non-conversation screen (`HEADER_H`, with the
        // divider this screen shares with Rules/Routines). The hand-built
        // `px_4 py_2` row was ~39px, so switching screens moved every row below
        // the title down by 17px.
        .child(bits::screen_header(
            "Usage",
            Some(&window_caption),
            div().flex().items_center().gap_2().child(window_row).child(
                ghost_button("Refresh")
                    .id("refresh-usage")
                    .on_click(move |_, _, _: &mut App| {
                        let _ = commands_refresh.send(Command::FetchUsage {
                            since: Some(since_for(since, Utc::now())),
                        });
                    }),
            ),
        ))
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
            div().child(ghost_button("Retry").id("retry-usage").on_click(
                move |_, _, _: &mut App| {
                    let _ = cmds.send(Command::FetchUsage {
                        since: Some(since_for(hours, Utc::now())),
                    });
                },
            )),
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
        // Human labels, not the wire format. This screen used to print
        // `2026-11-14T01:53:02Z` — the RFC 3339 string the brain was *asked*
        // with — next to a transcript that says "Today 8:41 AM" for the same
        // kind of instant. A raw timestamp is not wrong, but it is a second
        // clock convention in one window, and the reader has to do the
        // arithmetic in a zone they did not pick.
        .child(bits::kv(
            "from",
            crate::time::moment_label_here(report.since),
        ))
        .child(bits::kv("to", crate::time::moment_label_here(report.until)))
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
                    // Truncating, not fixed and free: a long `@name` used to run
                    // into the first numeric column with no ellipsis to stop it.
                    div()
                        .w(px(BOT_LABEL_W))
                        .flex_none()
                        .min_w_0()
                        .truncate()
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
        .child(
            div()
                .w(px(BOT_LABEL_W))
                .flex_none()
                .min_w_0()
                .truncate()
                .child("bot"),
        )
        .child(heading_cell("in"))
        .child(heading_cell("out"))
        .child(heading_cell("model"))
        .child(heading_cell("tools"))
}

fn heading_cell(label: &str) -> gpui::Div {
    div()
        .w(px(NUMERIC_COL_W))
        .flex_none()
        .text_right()
        .text_xs()
        .text_color(rgb(theme::TEXT_DIM))
        .child(label.to_string())
}

fn cell(value: String) -> gpui::Div {
    div()
        .w(px(NUMERIC_COL_W))
        .flex_none()
        // Right-aligned like its heading: a left-aligned fixed column makes a
        // 4-digit count and a 7-digit one start at the same x, so the digits
        // move under each other from row to row.
        .text_right()
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

    /// A numeric column has to be wide enough for the widest value it can show.
    /// It was 72px and left-aligned, while [`thousands`] emits `12,345,678`
    /// (~76px at the row's 14px) — so a seven-figure count was wider than its own
    /// column and the numbers did not hold a line.
    #[test]
    fn a_numeric_column_is_wide_enough_for_what_it_shows() {
        let widest_realistic = thousands(12_345_678);
        assert_eq!(widest_realistic, "12,345,678");
        assert!(
            numeric_text_width(&widest_realistic) > 72.0,
            "the old 72px column was narrower than {}: it never fit",
            widest_realistic
        );
        assert!(
            numeric_text_width(&widest_realistic) <= NUMERIC_COL_W,
            "{} needs {:.1}px but the column is {NUMERIC_COL_W}px",
            widest_realistic,
            numeric_text_width(&widest_realistic)
        );
        // Every value the screen can render up to nine digits, commas included.
        for value in [0u64, 7, 999, 1_000, 12_345, 1_288_000, 999_999_999] {
            let cell = thousands(value);
            assert!(
                numeric_text_width(&cell) <= NUMERIC_COL_W,
                "{cell} does not fit in a {NUMERIC_COL_W}px column"
            );
        }
    }

    /// The heading row and the data rows take their widths from the same
    /// constants, so a heading cannot drift off the column it names. The label
    /// cell is the truncating one: it is the only cell whose content is arbitrary
    /// text rather than a formatted number.
    #[test]
    fn the_heading_row_and_the_data_rows_share_their_column_widths() {
        assert_eq!(BOT_LABEL_W, 220.0);
        assert_eq!(
            NUMERIC_COL_W, 88.0,
            "wide enough for `12,345,678` and then some"
        );
        // A value that overflows the label cell is truncated, never pushed into
        // the numbers: the cell's width is fixed and it has an ellipsis.
        let long = bot_label(&BotUsage {
            bot_id: BotId::from_suffix("a-very-long-bot-identifier-that-will-not-fit"),
            bot_name: Some("a-very-long-bot-name-that-will-not-fit-in-220-pixels".into()),
            usage: Default::default(),
        });
        assert!(
            numeric_text_width(&long) > BOT_LABEL_W,
            "the case the cell must survive"
        );
    }
}

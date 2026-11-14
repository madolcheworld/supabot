//! The approval-rules screen: the saved answers behind "Always allow", and the
//! only place in the app where one can be read or removed.
//!
//! A rule is what keeps the same question from being asked twice. Until this
//! screen existed, the owner could create rules (the "Always allow" button on a
//! card writes one) and could never see them: the only way to find out what had
//! been auto-approved, or to unsave a rule a bot no longer needs, was to open the
//! SQLite file. A permission the owner cannot inspect is a permission the owner
//! cannot withdraw, so listing and deleting them is the whole point of the
//! screen — it is not a settings page that happens to mention rules.
//!
//! **The three states are kept apart**, per [`crate::state::Fetched`]: a list
//! nobody has fetched says so and offers to fetch it, a failed fetch shows the
//! reason with a Retry, and only a successful answer that happens to be empty
//! says "no rules saved yet". The first two are the common ones here — the live
//! socket does not carry rules, so this list is REST-only and can genuinely be
//! unknown while the rest of the app is live.

use std::sync::mpsc::Sender;

use gpui::{App, Entity, div, prelude::*, rgb};
use swarmbot_contracts::{ApprovalRule, RuleEffect, RuleScope, Surface};

use crate::proto::Command;
use crate::state::{AppState, Fetched};
use crate::theme;
use crate::time;
use crate::widgets::bits::{self, Tone};

pub fn render(s: &AppState, _state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    let commands_refresh = commands.clone();
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
                                .child("Approval rules"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child("saved answers that skip the card"),
                        ),
                )
                .child(
                    bits::button("Refresh", Tone::Ghost)
                        .id("refresh-rules")
                        .on_click(move |_, _, _: &mut App| {
                            let _ = commands_refresh.send(Command::FetchRules);
                        }),
                ),
        )
        .child(
            div()
                .id("rules-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_3()
                .px_4()
                .py_3()
                .child(match &s.approval_rules {
                    Fetched::Unknown => not_loaded(commands),
                    Fetched::Failed(reason) => failed(reason, commands),
                    Fetched::Ready(rules) if rules.is_empty() => empty(),
                    Fetched::Ready(rules) => rule_list(rules, commands),
                })
                .child(div().text_xs().text_color(rgb(theme::TEXT_DIM)).child(
                    "Rules are written by \"Always allow\" on an approval card. A rule with \
                             no matcher decides every request in its scope — that is a supported \
                             kill switch, and the card says so before saving one.",
                )),
        )
}

/// Nobody has asked the brain yet. Saying "no rules" here would be a claim about
/// a list that has not been read.
fn not_loaded(commands: &Sender<Command>) -> gpui::Div {
    let cmds = commands.clone();
    div()
        .text_sm()
        .text_color(rgb(theme::TEXT_MUTED))
        .child("Not loaded yet.")
        .child(
            div().mt_2().child(
                bits::button("Load rules", Tone::Ghost)
                    .id("load-rules")
                    .on_click(move |_, _, _: &mut App| {
                        let _ = cmds.send(Command::FetchRules);
                    }),
            ),
        )
}

/// The fetch broke. The reason is shown verbatim, with a way to try again —
/// never folded into an empty list.
fn failed(reason: &str, commands: &Sender<Command>) -> gpui::Div {
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
                .child("Could not read the approval rules"),
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
                    .id("retry-rules")
                    .on_click(move |_, _, _: &mut App| {
                        let _ = cmds.send(Command::FetchRules);
                    }),
            ),
        )
}

/// The honest empty: the list was read and holds nothing.
fn empty() -> gpui::Div {
    div()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .p_4()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::TEXT))
                .child("No approval rules saved"),
        )
        .child(div().text_xs().text_color(rgb(theme::TEXT_MUTED)).child(
            "Every gated action currently asks, unless a backend's ceiling lets it through.",
        ))
}

fn rule_list(rules: &[ApprovalRule], commands: &Sender<Command>) -> gpui::Div {
    let mut list = div().w_full().flex().flex_col().gap_2();
    for rule in rules {
        list = list.child(rule_card(rule, commands));
    }
    list
}

/// The word the screen, the palette row and the badge all use for an effect.
pub fn effect_label(effect: RuleEffect) -> &'static str {
    match effect {
        RuleEffect::Allow => "allow",
        RuleEffect::Ask => "ask",
        RuleEffect::Deny => "deny",
    }
}

fn effect_tone(effect: RuleEffect) -> u32 {
    match effect {
        RuleEffect::Allow => theme::SUCCESS,
        RuleEffect::Ask => theme::WARNING,
        RuleEffect::Deny => theme::DANGER,
    }
}

fn rule_card(rule: &ApprovalRule, commands: &Sender<Command>) -> gpui::Div {
    let cmds = commands.clone();
    let id = rule.id.clone();
    let effect = effect_label(rule.effect);
    let tone = effect_tone(rule.effect);

    let scope = match &rule.scope {
        RuleScope::Global => "every bot".to_string(),
        RuleScope::Bot(bot) => format!("bot {}", bot.as_str()),
    };

    let mut card = div()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .p_3()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(bits::badge(effect, theme::SURFACE_RAISED, tone))
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(theme::TEXT))
                        .child(matcher_title(rule)),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(time::day_label_here(rule.created_at)),
                )
                .child(
                    bits::button("Delete", Tone::Danger)
                        .id(gpui::SharedString::from(format!(
                            "delete-rule-{}",
                            rule.id.as_str()
                        )))
                        .on_click(move |_, _, _: &mut App| {
                            let _ = cmds.send(Command::DeleteRule(id.clone()));
                        }),
                ),
        )
        .child(bits::kv("scope", scope))
        .child(bits::kv("rule id", rule.id.as_str().to_string()));

    if let Some(tool) = rule.tool.as_deref() {
        card = card.child(bits::kv("tool", tool.to_string()));
    }
    if let Some(surface) = rule.surface {
        card = card.child(bits::kv("surface", surface_label(surface).to_string()));
    }
    if let Some(prefix) = rule.command_prefix.as_deref() {
        card = card.child(bits::kv("target starts with", prefix.to_string()));
    }
    if matches!(rule.effect, RuleEffect::Allow | RuleEffect::Deny)
        && rule.surface.is_none()
        && rule.tool.is_none()
        && rule.command_prefix.is_none()
    {
        // The kill-switch shape: accepted by the brain on purpose, and the one a
        // saved rule most needs to be loud about, because an owner forgets it.
        card = card.child(
            div()
                .text_xs()
                .text_color(rgb(theme::DANGER))
                .child("no matcher: this rule decides every request in its scope"),
        );
    }
    card
}

/// The one-line title of a rule: what it matches, in the owner's words.
///
/// A rule with no matcher is described as what it is — everything — instead of
/// being rendered as a row of empty labels, which reads like a broken rule
/// rather than a broad one.
pub fn matcher_title(rule: &ApprovalRule) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(surface) = rule.surface {
        parts.push(surface_label(surface).to_string());
    }
    if let Some(tool) = rule.tool.as_deref() {
        parts.push(format!("tool `{tool}`"));
    }
    if let Some(prefix) = rule.command_prefix.as_deref() {
        parts.push(format!("starting with `{prefix}`"));
    }
    if parts.is_empty() {
        "every request".to_string()
    } else {
        parts.join(" · ")
    }
}

fn surface_label(surface: Surface) -> &'static str {
    // The contract already names these (`Surface::as_str`), and the card, the
    // rule and the wire all have to spell them the same way, so the name is
    // taken from there rather than re-typed here.
    surface.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::{BotId, RuleId};

    fn rule(scope: RuleScope, effect: RuleEffect) -> ApprovalRule {
        ApprovalRule {
            id: RuleId::new(),
            scope,
            surface: None,
            tool: None,
            command_prefix: None,
            effect,
            created_at: chrono::Utc::now(),
        }
    }

    /// A rule with no matcher must be *described* as deciding everything. The
    /// screen is the owner's only view of a rule, so a title that reads as an
    /// empty row is the same failure as not listing it at all.
    #[test]
    fn a_matcherless_rule_is_named_as_every_request() {
        let rule = rule(RuleScope::Global, RuleEffect::Deny);
        assert_eq!(matcher_title(&rule), "every request");
    }

    /// The title names every matcher that is set, in the order the rule applies
    /// them, so the owner can read the rule back without opening the database.
    #[test]
    fn a_rule_title_names_each_matcher_it_has() {
        let mut rule = rule(RuleScope::Global, RuleEffect::Allow);
        rule.surface = Some(Surface::Shell);
        rule.tool = Some("shell".into());
        rule.command_prefix = Some("cargo".into());
        assert_eq!(
            matcher_title(&rule),
            "shell · tool `shell` · starting with `cargo`"
        );

        // A bot-scoped rule says whose it is: "every bot" would be a lie.
        let scoped = ApprovalRule {
            scope: RuleScope::Bot(BotId::from_suffix("dev")),
            ..rule
        };
        assert_eq!(
            matcher_title(&scoped),
            "shell · tool `shell` · starting with `cargo`"
        );
    }

    /// The three states of a list are distinct in the copy the screen shows, so
    /// "not fetched" and "failed" can never read as "you have no rules".
    #[test]
    fn the_empty_state_is_only_for_an_answer_that_was_actually_read() {
        let mut state = AppState::new("http://brain.test");
        assert!(matches!(state.approval_rules, Fetched::Unknown));

        // A failure stays a failure, with its reason.
        crate::reducer::apply_result(
            &mut state,
            crate::proto::CommandResult::Rules(Err(swarmbot_client::ClientError::Transport {
                url: "http://brain.test".into(),
                reason: "connection refused".into(),
            })),
        );
        match &state.approval_rules {
            Fetched::Failed(reason) => assert!(reason.contains("connection refused"), "{reason}"),
            other => panic!("a failed fetch must not read as a list: {other:?}"),
        }

        // A real answer replaces it, and an empty answer is the only empty.
        crate::reducer::apply_result(
            &mut state,
            crate::proto::CommandResult::Rules(Ok(Vec::new())),
        );
        assert!(matches!(&state.approval_rules, Fetched::Ready(rules) if rules.is_empty()));
    }
}

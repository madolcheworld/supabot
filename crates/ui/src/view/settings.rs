//! The Settings screen: the backend list with a **Probe** button that shows the
//! result verbatim (including a gateway's own message, e.g. an inferhub 402 bid
//! line), and the brain-wide [`Settings`] the owner can change — the bot
//! messaging kill switch, hop/tool-round limits, auto-approve for read-only, and
//! the safety ceiling.
//!
//! Editing happens on a draft ([`AppState::settings_draft`]); **Save** diffs it
//! against the last-saved settings ([`diff`]) and PATCHes only the keys that
//! changed, so an untouched ceiling is never rewritten. The diff is a pure
//! function a `#[gpui::test]` asserts on.

use std::sync::mpsc::Sender;

use gpui::{Entity, SharedString, div, prelude::*, px, rgb};
use serde_json::{Map, Value, json};
use swarmbot_contracts::{BackendKind, PolicyCeiling, Settings};

use crate::proto::Command;
use crate::state::AppState;
use crate::theme;
use crate::widgets::bits::{self, Tone};

/// The keys Save will PATCH, computed by diffing `saved` against `draft`. Only
/// changed scalar keys and a changed `safety` map are included; an unchanged
/// setting is omitted so the brain's merge cannot clobber it.
pub fn diff(saved: &Settings, draft: &Settings) -> Value {
    let mut map = Map::new();
    if saved.bot_messaging != draft.bot_messaging {
        map.insert("bot_messaging".into(), json!(draft.bot_messaging));
    }
    if saved.max_hops != draft.max_hops {
        map.insert("max_hops".into(), json!(draft.max_hops));
    }
    if saved.max_tool_rounds != draft.max_tool_rounds {
        map.insert("max_tool_rounds".into(), json!(draft.max_tool_rounds));
    }
    if saved.auto_approve_read_only != draft.auto_approve_read_only {
        map.insert(
            "auto_approve_read_only".into(),
            json!(draft.auto_approve_read_only),
        );
    }
    if saved.transcript_mirror != draft.transcript_mirror {
        map.insert("transcript_mirror".into(), json!(draft.transcript_mirror));
    }
    if saved.safety != draft.safety {
        map.insert(
            "safety".into(),
            serde_json::to_value(&draft.safety).unwrap_or_else(|_| json!({})),
        );
    }
    Value::Object(map)
}

pub fn render(s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(
            div()
                .px_4()
                .py_2()
                .text_lg()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::TEXT))
                .child("Settings"),
        )
        .child(
            div()
                .id("settings-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_4()
                .px_4()
                .py_3()
                .child(backends_section(s, commands))
                .child(roster_skills_section(s))
                .child(settings_section(s, state, commands)),
        )
}

fn backends_section(s: &AppState, commands: &Sender<Command>) -> gpui::Div {
    let commands_probe = commands.clone();
    let mut section = div().w_full().flex().flex_col().gap_2().child(
        div()
            .flex()
            .items_center()
            .child(div().flex_1().child(bits::section_label("BACKENDS")))
            .child(
                bits::button("Probe", Tone::Ghost)
                    .id("probe-backends")
                    .on_click(move |_, _, _: &mut gpui::App| {
                        let _ = commands_probe.send(Command::ProbeBackends);
                    }),
            ),
    );

    let has_real = s
        .backends
        .iter()
        .any(|b| b.id != "fake" && b.kind != BackendKind::Fake);
    let visible_backends: Vec<_> = if has_real {
        s.backends
            .iter()
            .filter(|b| b.id != "fake" && b.kind != BackendKind::Fake)
            .collect()
    } else {
        s.backends.iter().collect()
    };

    if visible_backends.is_empty() {
        return section.child(
            div()
                .text_sm()
                .text_color(rgb(theme::TEXT_DIM))
                .child("no backends configured"),
        );
    }

    for backend in visible_backends {
        let probe = s
            .probes
            .as_ref()
            .and_then(|probes| probes.iter().find(|p| p.backend == backend.id));
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
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgb(theme::TEXT))
                            .child(backend.id.clone()),
                    )
                    .child(bits::badge(
                        backend_kind_label(backend.kind),
                        theme::SURFACE_RAISED,
                        theme::TEXT_MUTED,
                    )),
            );

        // Only rows with something to say: an ACP backend has no base_url, and
        // a wall of em-dashes is exactly what made these cards unreadable.
        if let Some(model) = backend.model.as_deref().filter(|m| !m.is_empty()) {
            card = card.child(bits::kv("model", model.to_string()));
        }
        if let Some(url) = backend.base_url.as_deref().filter(|u| !u.is_empty()) {
            card = card.child(bits::kv("base url", url.to_string()));
        }
        if let Some(env) = backend.api_key_env.as_deref().filter(|e| !e.is_empty()) {
            card = card.child(bits::kv("api key from", env.to_string()));
        }
        // How this backend connects: an ACP agent's command line, a harness's
        // binary, its working dir and permission mode. Secret-bearing values
        // arrive masked from the brain (`[hidden]`) and are shown as names.
        let rows = connection_rows(&backend.options);
        for (label, value) in rows {
            card = card.child(bits::kv(&label, value));
        }
        if backend.model.is_none()
            && backend.base_url.is_none()
            && backend.api_key_env.is_none()
            && backend.options.is_empty()
        {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("nothing to configure — always available"),
            );
        }

        if let Some(probe) = probe {
            let color = if probe.ok {
                theme::SUCCESS
            } else {
                theme::DANGER
            };
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(color)).child(if probe.ok {
                        "probe: reachable"
                    } else {
                        "probe: failed"
                    }))
                    // The detail is shown verbatim — including a gateway's own
                    // message (an inferhub 402 bid line, say). Never paraphrased.
                    .children(probe.detail.as_ref().map(|detail| {
                        div()
                            .w_full()
                            .rounded_sm()
                            .bg(rgb(theme::CODE_BG))
                            .px_2()
                            .py_1()
                            .font_family("DejaVu Sans Mono")
                            .text_size(px(11.5))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(detail.clone())
                    })),
            );
        } else {
            card = card.child(
                div()
                    .text_xs()
                    .italic()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child(
                        "not probed yet — press Probe (an ACP agent gets spawned; no model call)",
                    ),
            );
        }
        section = section.child(card);
    }
    section
}

fn roster_skills_section(s: &AppState) -> gpui::Div {
    let mut section = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(bits::section_label("ROSTER & SKILLS"));

    if s.bots.is_empty() {
        return section.child(
            div()
                .text_sm()
                .text_color(rgb(theme::TEXT_DIM))
                .child("no bots configured"),
        );
    }

    for bot in &s.bots {
        let mut card = div()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
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
                            .child(bits::avatar(
                                theme::bot_color(bot),
                                bits::initial(&bot.label),
                                theme::space::AVATAR,
                            ))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(theme::TEXT))
                                    .child(bot.label.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(format!("@{}", bot.name)),
                            ),
                    )
                    .child(bits::badge(
                        bot.status.label(),
                        theme::SURFACE_RAISED,
                        theme::TEXT_MUTED,
                    )),
            )
            .child(bits::kv("backend", bot.backend.clone()));

        if let Some(model) = &bot.model {
            card = card.child(bits::kv("model override", model.clone()));
        }

        // Skills display
        let skills_div = if bot.skills.is_empty() {
            div()
                .text_xs()
                .italic()
                .text_color(rgb(theme::TEXT_DIM))
                .child("no specialized skills assigned (baseline persona)")
        } else {
            div().flex().flex_wrap().gap_1().children(
                bot.skills
                    .iter()
                    .map(|skill| bits::badge(skill.clone(), theme::SURFACE_RAISED, theme::ACCENT)),
            )
        };

        card = card.child(
            div()
                .flex()
                .items_baseline()
                .gap_2()
                .text_sm()
                .child(
                    div()
                        .w(px(120.0))
                        .flex_none()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("skills"),
                )
                .child(div().flex_1().child(skills_div)),
        );

        section = section.child(card);
    }

    section
}

/// The card rows for a backend's redacted `options`: the spawn command line
/// (`command` + `args`) merged into one **runs** row, every other option
/// shown under its own name. Pure, so a test can pin the wording.
pub fn connection_rows(options: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    if let Some(command) = options.get("command").and_then(Value::as_str) {
        let args = options
            .get("args")
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        rows.push((
            "runs".to_string(),
            if args.is_empty() {
                command.to_string()
            } else {
                format!("{command} {args}")
            },
        ));
    }
    for (name, value) in options {
        if name == "command" || name == "args" {
            continue;
        }
        rows.push((name.clone(), option_display(value)));
    }
    rows
}

/// One option value as a readable cell. An all-`[hidden]` object (the ACP
/// `env` map) shows just its variable names — the values were masked by the
/// brain and there is nothing honest left to render.
pub fn option_display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => {
            if *flag {
                "on".to_string()
            } else {
                "off".to_string()
            }
        }
        Value::Number(number) => number.to_string(),
        Value::Array(items) => items
            .iter()
            .map(option_display)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(map) if !map.is_empty() && map.values().all(is_hidden) => {
            map.keys().cloned().collect::<Vec<_>>().join(", ") + " (values hidden)"
        }
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| format!("{key}={}", option_display(value)))
            .collect::<Vec<_>>()
            .join(", "),
        Value::Null => "—".to_string(),
    }
}

fn is_hidden(value: &Value) -> bool {
    value.as_str() == Some("[hidden]")
}

fn settings_section(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
) -> gpui::Div {
    let Some(draft) = s.settings_draft.as_ref().or(s.settings.as_ref()) else {
        let commands_fetch = commands.clone();
        return div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(bits::section_label("BRAIN SETTINGS"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("settings not loaded"),
            )
            .child(
                bits::button("Load settings", Tone::Ghost)
                    .id("load-settings")
                    .on_click(move |_, _, _: &mut gpui::App| {
                        let _ = commands_fetch.send(Command::FetchSettings);
                    }),
            );
    };

    let mut section = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(bits::section_label("BRAIN SETTINGS"));

    section = section
        .child(toggle_row(
            "bot messaging (kill switch)",
            draft.bot_messaging,
            state,
            |d| d.bot_messaging = !d.bot_messaging,
        ))
        .child(toggle_row(
            "auto-approve read-only tools",
            draft.auto_approve_read_only,
            state,
            |d| d.auto_approve_read_only = !d.auto_approve_read_only,
        ))
        .child(toggle_row(
            "mirror transcripts to disk",
            draft.transcript_mirror,
            state,
            |d| d.transcript_mirror = !d.transcript_mirror,
        ))
        .child(stepper_row(
            "max hops",
            draft.max_hops as i64,
            state,
            |d, delta| {
                d.max_hops = (d.max_hops as i64 + delta).clamp(1, 32) as u8;
            },
        ))
        .child(stepper_row(
            "max tool rounds",
            draft.max_tool_rounds as i64,
            state,
            |d, delta| {
                d.max_tool_rounds = (d.max_tool_rounds as i64 + delta).clamp(1, 256) as u32;
            },
        ))
        .child(ceiling_row(draft.safety.default, state));

    // Save: PATCH only what changed.
    let commands_save = commands.clone();
    let state_save = state.clone();
    section = section.child(
        div().flex().justify_end().gap_2().child(
            bits::button("Save", Tone::Primary)
                .id("save-settings")
                .on_click(move |_, _, cx: &mut gpui::App| {
                    let patch = {
                        let st = state_save.read(cx);
                        match (&st.settings, &st.settings_draft) {
                            (Some(saved), Some(draft)) => diff(saved, draft),
                            _ => Value::Object(Map::new()),
                        }
                    };
                    if patch.as_object().is_some_and(|m| m.is_empty()) {
                        state_save.update(cx, |st, cx| {
                            st.toast = Some("no changes to save".to_string());
                            cx.notify();
                        });
                    } else {
                        let _ = commands_save.send(Command::UpdateSettings(patch));
                    }
                }),
        ),
    );
    section
}

fn toggle_row<F>(label: &str, value: bool, state: &Entity<AppState>, setter: F) -> gpui::AnyElement
where
    F: Fn(&mut Settings) + 'static,
{
    let state_h = state.clone();
    div()
        .id(SharedString::from(format!("toggle-{label}")))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .child(bits::dot(
            if value { theme::SUCCESS } else { theme::BORDER },
            px(12.0),
        ))
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(rgb(theme::TEXT))
                .child(label.to_string()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(if value { "on" } else { "off" }),
        )
        .on_click(move |_, _, cx: &mut gpui::App| {
            state_h.update(cx, |s, cx| {
                if let Some(draft) = &mut s.settings_draft {
                    setter(draft);
                }
                cx.notify();
            });
        })
        .into_any_element()
}

fn stepper_row<F>(label: &str, value: i64, state: &Entity<AppState>, setter: F) -> gpui::Div
where
    F: Fn(&mut Settings, i64) + Clone + 'static,
{
    let state_dec = state.clone();
    let state_inc = state.clone();
    let setter_dec = setter.clone();
    let setter_inc = setter;
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(rgb(theme::TEXT))
                .child(label.to_string()),
        )
        .child(
            bits::button("−", Tone::Ghost)
                .id(SharedString::from(format!("dec-{label}")))
                .on_click(move |_, _, cx: &mut gpui::App| {
                    state_dec.update(cx, |s, cx| {
                        if let Some(draft) = &mut s.settings_draft {
                            setter_dec(draft, -1);
                        }
                        cx.notify();
                    });
                }),
        )
        .child(
            div()
                .w(px(48.0))
                .text_center()
                .text_sm()
                .text_color(rgb(theme::TEXT))
                .child(value.to_string()),
        )
        .child(
            bits::button("+", Tone::Ghost)
                .id(SharedString::from(format!("inc-{label}")))
                .on_click(move |_, _, cx: &mut gpui::App| {
                    state_inc.update(cx, |s, cx| {
                        if let Some(draft) = &mut s.settings_draft {
                            setter_inc(draft, 1);
                        }
                        cx.notify();
                    });
                }),
        )
}

fn ceiling_row(current: PolicyCeiling, state: &Entity<AppState>) -> gpui::Div {
    let mut row = div().flex().items_center().gap_2().px_2().py_1().child(
        div()
            .flex_1()
            .text_sm()
            .text_color(rgb(theme::TEXT))
            .child("default safety ceiling"),
    );
    for ceiling in [
        PolicyCeiling::Never,
        PolicyCeiling::Ask,
        PolicyCeiling::Always,
    ] {
        let active = ceiling == current;
        let state_h = state.clone();
        row = row.child(
            bits::button(
                ceiling_label(ceiling),
                if active { Tone::Primary } else { Tone::Ghost },
            )
            .id(SharedString::from(format!(
                "ceiling-{}",
                ceiling_label(ceiling)
            )))
            .on_click(move |_, _, cx: &mut gpui::App| {
                state_h.update(cx, |s, cx| {
                    if let Some(draft) = &mut s.settings_draft {
                        draft.safety.default = ceiling;
                    }
                    cx.notify();
                });
            }),
        );
    }
    row
}

fn ceiling_label(ceiling: PolicyCeiling) -> &'static str {
    match ceiling {
        PolicyCeiling::Never => "never",
        PolicyCeiling::Ask => "ask",
        PolicyCeiling::Always => "always",
    }
}

pub(crate) fn backend_kind_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::OpenAiCompat => "openai-compat",
        BackendKind::ClaudeCli => "claude-cli",
        BackendKind::Acp => "acp",
        BackendKind::Fake => "fake",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::PolicyCeiling;

    #[test]
    fn save_patches_only_what_changed() {
        let saved = Settings::default();
        let mut draft = saved.clone();
        assert!(
            diff(&saved, &draft).as_object().expect("object").is_empty(),
            "no edits -> an empty patch (nothing is rewritten)"
        );

        draft.bot_messaging = false;
        draft.max_hops = 3;
        let patch = diff(&saved, &draft);
        assert_eq!(patch["bot_messaging"], false);
        assert_eq!(patch["max_hops"], 3);
        assert!(
            patch.get("max_tool_rounds").is_none(),
            "an untouched key is omitted so the brain's merge cannot clobber it"
        );
        assert!(patch.get("safety").is_none());
    }

    #[test]
    fn a_changed_ceiling_is_included_whole() {
        let saved = Settings::default();
        let mut draft = saved.clone();
        draft.safety.default = PolicyCeiling::Never;
        let patch = diff(&saved, &draft);
        assert!(
            patch.get("safety").is_some(),
            "a changed safety map is sent"
        );
        // The kill switch and limits are untouched, so they stay out.
        assert!(patch.get("bot_messaging").is_none());
        assert!(patch.get("max_hops").is_none());
    }

    #[test]
    fn connection_rows_merge_the_command_line() {
        let options: serde_json::Map<String, Value> = serde_json::from_value(json!({
            "command": "opencode",
            "args": ["acp"],
            "permission_mode": "reject",
            "env": {"ANTHROPIC_API_KEY": "[hidden]"},
            "inherit_env": true,
        }))
        .expect("map");
        let rows = connection_rows(&options);
        assert_eq!(rows[0], ("runs".to_string(), "opencode acp".to_string()));
        let labels: Vec<&str> = rows.iter().map(|(label, _)| label.as_str()).collect();
        // command+args are merged into `runs`, never repeated separately.
        assert!(!labels.contains(&"command") && !labels.contains(&"args"));
        assert!(labels.contains(&"permission_mode"));
        let env = rows.iter().find(|(label, _)| label == "env").expect("env");
        assert_eq!(env.1, "ANTHROPIC_API_KEY (values hidden)");
        let inherit = rows
            .iter()
            .find(|(label, _)| label == "inherit_env")
            .expect("inherit_env");
        assert_eq!(inherit.1, "on");
    }

    #[test]
    fn option_display_reads_every_json_shape() {
        assert_eq!(option_display(&json!("x")), "x");
        assert_eq!(option_display(&json!(true)), "on");
        assert_eq!(option_display(&json!(false)), "off");
        assert_eq!(option_display(&json!(3)), "3");
        assert_eq!(option_display(&json!(["a", "b"])), "a, b");
        assert_eq!(option_display(&json!({"k": "v"})), "k=v");
        assert_eq!(option_display(&Value::Null), "—");
        // A masked map shows names only — the brain already hid the values.
        assert_eq!(
            option_display(&json!({"A": "[hidden]", "B": "[hidden]"})),
            "A, B (values hidden)"
        );
    }
}

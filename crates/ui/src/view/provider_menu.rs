//! The provider menu: where the owner chooses **which configured backend a
//! bot runs on** (left column) and **which model** (right column, with a
//! search box), per bot.
//!
//! It exists because the answer to "where do I pick the provider?" used to be
//! "edit `config.toml` and restart" — the Settings screen could only *show*
//! backends. The left column lists every backend the brain actually holds
//! (the same redacted rows the Settings screen renders); the right column
//! lists the models *that backend itself advertises* — an OpenAI-compatible
//! gateway's `/models`, an ACP agent's session model state — fetched on
//! demand and cached in [`AppState::models`]. Clicking Apply PATCHes
//! `/api/bots/{id}`; the fresh bot state comes back as a live `BotUpsert`, so
//! the header label updates by itself.
//!
//! Honesty rules this panel obeys:
//! * a backend that advertises **no** model list (opencode's ACP server, the
//!   `claude -p` skeleton) gets an empty right column that says so, plus the
//!   search box as a free-text escape hatch — never a fabricated menu;
//! * only a **direct** conversation gets the menu: it configures exactly one
//!   bot. A group's members can sit on different providers, and one dropdown
//!   cannot honestly speak for all of them.

use std::sync::mpsc::Sender;

use gpui::{Entity, SharedString, div, prelude::*, px, rgb};
use swarmbot_client::ModelOption;

use crate::proto::Command;
use crate::state::AppState;
use crate::theme;
use crate::view::conversation::dm_bot;
use crate::view::settings::backend_kind_label;
use crate::widgets::bits::{self, Tone};
use crate::widgets::text_input::TextInput;

/// Case-insensitive substring filter over model ids and labels; an empty
/// query lists everything. Pure so a test can pin the search behaviour.
pub fn filter_models(query: &str, models: &[ModelOption]) -> Vec<ModelOption> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return models.to_vec();
    }
    models
        .iter()
        .filter(|model| {
            model.id.to_lowercase().contains(&q)
                || model
                    .label
                    .as_deref()
                    .is_some_and(|label| label.to_lowercase().contains(&q))
        })
        .cloned()
        .collect()
}

/// The menu overlay, or `None` when it is closed or the open conversation has
/// no single bot to configure.
pub fn render(
    s: &AppState,
    state: &Entity<AppState>,
    search: &Entity<TextInput>,
    commands: &Sender<Command>,
    cx: &gpui::App,
) -> Option<gpui::Div> {
    if !s.bot_menu_open {
        return None;
    }
    let conversation = s.open_conversation()?;
    let bot = s
        .menu_bot
        .as_ref()
        .and_then(|id| s.bot(id).cloned())
        .or_else(|| dm_bot(s, conversation))?;
    let selected_backend = s
        .menu_backend
        .clone()
        .unwrap_or_else(|| bot.backend.clone());

    // --- optional bot switcher for group conversations ---------------------
    let mut bot_switcher: Option<gpui::Div> = None;
    if let swarmbot_contracts::ConversationKind::Group { members, .. } = &conversation.kind {
        let mut row = div().flex().items_center().gap_1().flex_wrap();
        for m_id in members {
            if let Some(member_bot) = s.bot(m_id) {
                let is_active_bot = member_bot.id == bot.id;
                let m_id_clone = member_bot.id.clone();
                let m_backend_clone = member_bot.backend.clone();
                let m_model_clone = member_bot.model.clone();
                let state_h = state.clone();
                let search_h = search.clone();
                let commands_h = commands.clone();

                let pill_tone = if is_active_bot {
                    Tone::Primary
                } else {
                    Tone::Ghost
                };
                let pill_label = format!("@{}", member_bot.name);
                row = row.child(
                    bits::button(&pill_label, pill_tone)
                        .id(SharedString::from(format!("bot-switch-{}", member_bot.id)))
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            let mut fetch: Option<String> = None;
                            state_h.update(cx, |s, cx| {
                                s.menu_bot = Some(m_id_clone.clone());
                                s.menu_backend = Some(m_backend_clone.clone());
                                s.menu_model = m_model_clone.clone();
                                if !s.models.contains_key(&m_backend_clone) {
                                    fetch = Some(m_backend_clone.clone());
                                }
                                cx.notify();
                            });
                            let initial = m_model_clone.clone().unwrap_or_default();
                            search_h.update(cx, |input, cx| input.set_content(&initial, cx));
                            let _ = commands_h.send(Command::FetchBackends);
                            if let Some(backend) = fetch {
                                let _ = commands_h.send(Command::FetchModels(backend));
                            }
                        }),
                );
            }
        }
        bot_switcher = Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(bits::section_label("CONFIGURE BOT"))
                .child(row),
        );
    }

    // --- left column: providers -------------------------------------------
    let mut providers = div().w(px(230.0)).flex().flex_col().gap_1();
    for backend in &s.backends {
        let is_current = backend.id == bot.backend;
        let is_selected = backend.id == selected_backend;
        let backend_id = backend.id.clone();
        let state_h = state.clone();
        let search_prov = search.clone();
        let commands_fetch = commands.clone();
        providers = providers.child(
            div()
                .id(SharedString::from(format!("provider-pick-{}", backend.id)))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(is_selected, |d| d.bg(rgb(theme::SURFACE)))
                .on_click(move |_, _, cx: &mut gpui::App| {
                    let mut fetch: Option<String> = None;
                    state_h.update(cx, |s, cx| {
                        s.menu_backend = Some(backend_id.clone());
                        // A different provider means a different model menu;
                        // the previous pick must not leak across.
                        s.menu_model = None;
                        if !s.models.contains_key(&backend_id) {
                            fetch = Some(backend_id.clone());
                        }
                        cx.notify();
                    });
                    search_prov.update(cx, |input, cx| input.clear(cx));
                    if let Some(backend) = fetch {
                        let _ = commands_fetch.send(Command::FetchModels(backend));
                    }
                })
                .child(bits::dot(
                    if is_current {
                        theme::SUCCESS
                    } else {
                        theme::BORDER
                    },
                    px(8.0),
                ))
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(rgb(theme::TEXT))
                        .child(backend.id.clone()),
                )
                .child(bits::badge(
                    backend_kind_label(backend.kind),
                    theme::SURFACE_RAISED,
                    theme::TEXT_MUTED,
                )),
        );
    }

    // --- right column: models of the selected provider ---------------------
    let query = search.read(cx).content().trim().to_string();
    let effective_model = if !query.is_empty() {
        Some(query.clone())
    } else {
        None
    };
    let listed = s.models.get(&selected_backend);
    let mut models_col = div()
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(bits::section_label(&format!(
            "MODELS OF `{selected_backend}`"
        )))
        .child(search.clone());

    // Fixed height, not max: the footer buttons (Apply/Clear/Close) must not
    // jump when the list goes from "loading" to one row to three hundred —
    // a moving target is exactly what makes this menu mis-clickable.
    let mut list = div()
        .id("provider-model-list")
        .w_full()
        .h(px(240.0))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap_1();

    // Default option at the top of the model list:
    let is_default_picked = effective_model.is_none();
    let state_def = state.clone();
    let search_def = search.clone();
    list = list.child(
        div()
            .id(SharedString::from(format!(
                "model-pick-{}-default",
                selected_backend
            )))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(is_default_picked, |d| d.bg(rgb(theme::SURFACE)))
            .on_click(move |_, _, cx: &mut gpui::App| {
                search_def.update(cx, |input, cx| input.clear(cx));
                state_def.update(cx, |s, cx| {
                    s.menu_model = None;
                    cx.notify();
                });
            })
            .child(bits::dot(
                if is_default_picked {
                    theme::SUCCESS
                } else {
                    theme::BORDER
                },
                px(8.0),
            ))
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(rgb(theme::TEXT))
                    .child("(Default) Use backend's configured model"),
            ),
    );

    match listed {
        None => {
            list = list.child(
                div()
                    .text_xs()
                    .italic()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("loading the model list…"),
            );
        }
        Some(Err(reason)) => {
            let backend_retry = selected_backend.clone();
            let commands_retry = commands.clone();
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::DANGER))
                            .child(format!("could not load the model list: {reason}")),
                    )
                    .child(
                        bits::button("Retry", Tone::Ghost)
                            .id(SharedString::from(format!(
                                "provider-retry-{backend_retry}"
                            )))
                            .on_click(move |_, _, _: &mut gpui::App| {
                                let _ = commands_retry
                                    .send(Command::FetchModels(backend_retry.clone()));
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_DIM))
                            .child("You can type any model ID in the box above to use it."),
                    ),
            );
        }
        Some(Ok(mlist)) if mlist.models.is_empty() => {
            list = list.child(div().text_xs().text_color(rgb(theme::TEXT_DIM)).child(
                "this backend advertises no model list over ACP — set `options.models_command` \
                 (e.g. opencode models) to list them from the agent's own CLI, type a \
                 model id in the box above, or select (Default) above.",
            ));
        }
        Some(Ok(mlist)) => {
            let visible = filter_models(&query, &mlist.models);
            if visible.is_empty() && !query.is_empty() {
                list = list.child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(format!(
                            "no model in list matches “{query}” — will apply as custom model ID"
                        )),
                );
            }
            for model in visible {
                let is_picked = effective_model.as_deref() == Some(model.id.as_str());
                let state_h = state.clone();
                let search_row = search.clone();
                let model_id_click = model.id.clone();
                list =
                    list.child(
                        div()
                            .id(SharedString::from(format!(
                                "model-pick-{}-{}",
                                selected_backend, model.id
                            )))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .when(is_picked, |d| d.bg(rgb(theme::SURFACE)))
                            .on_click(move |_, _, cx: &mut gpui::App| {
                                search_row
                                    .update(cx, |input, cx| input.set_content(&model_id_click, cx));
                                state_h.update(cx, |s, cx| {
                                    s.menu_model = Some(model_id_click.clone());
                                    cx.notify();
                                });
                            })
                            .child(bits::dot(
                                if is_picked {
                                    theme::SUCCESS
                                } else {
                                    theme::BORDER
                                },
                                px(8.0),
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .text_color(rgb(theme::TEXT))
                                    .child(model.id.clone()),
                            )
                            .children(model.label.as_ref().filter(|l| **l != model.id).map(
                                |label| {
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_DIM))
                                        .child(label.clone())
                                },
                            )),
                    );
            }
        }
    }
    models_col = models_col.child(list);

    // --- footer: what Apply will do ----------------------------------------
    let summary = match &effective_model {
        Some(model) => format!("provider `{selected_backend}` · model `{model}`"),
        None => format!("provider `{selected_backend}` · model: the backend's default"),
    };
    // A list read from the agent's own CLI (or no list at all from an agent
    // without ACP model state) is informational: a pick is recorded but the
    // agent may keep its own configured model. Say so loudly.
    let pick_may_not_reach = matches!(listed, Some(Ok(mlist)) if !mlist.authoritative);
    let bot_id_apply = bot.id.clone();
    let backend_apply = selected_backend.clone();
    let commands_apply = commands.clone();
    let state_apply = state.clone();
    let search_apply = search.clone();

    let state_clear = state.clone();
    let search_clear = search.clone();
    let state_close = state.clone();

    Some(
        div()
            .absolute()
            .top(theme::space::HEADER_H)
            .left(px(12.0))
            .w(px(620.0))
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE_RAISED))
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(rgb(theme::TEXT))
                    .child(format!("Provider & model for @{}", bot.name)),
            )
            .child(div().text_xs().text_color(rgb(theme::TEXT_DIM)).child(
                "Left: which configured backend runs this bot. Right: the models that \
                         backend advertises. Takes effect on the next message.",
            ))
            .children(bot_switcher)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(bits::section_label("PROVIDER"))
                            .child(providers),
                    )
                    .child(models_col),
            )
            .child(div().w_full().h(px(1.0)).bg(rgb(theme::BORDER)))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(summary),
            )
            .when(
                pick_may_not_reach && effective_model.is_some(),
                |d| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::WARNING))
                            .child(
                                "warning: this backend does not accept model selection over ACP — the list is informational and the agent may keep its own configured model (opencode does). To make picks reach it, set options.model_env (e.g. OPENCODE_CONFIG_CONTENT = {model-template}) in the brain config.",
                            ),
                    )
                },
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        bits::button("Apply", Tone::Primary)
                            .id("provider-apply")
                            .on_click(move |_, _, cx: &mut gpui::App| {
                                let typed = search_apply.read(cx).content().trim().to_string();
                                let final_model = (!typed.is_empty()).then_some(typed);
                                let _ = commands_apply.send(Command::SetBotBackend {
                                    bot: bot_id_apply.clone(),
                                    backend: backend_apply.clone(),
                                });
                                let _ = commands_apply.send(Command::SetBotModel {
                                    bot: bot_id_apply.clone(),
                                    model: final_model,
                                });
                                state_apply.update(cx, |s, cx| {
                                    s.bot_menu_open = false;
                                    s.menu_bot = None;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        bits::button("Reset to default", Tone::Ghost)
                            .id("provider-clear-model")
                            .on_click(move |_, _, cx: &mut gpui::App| {
                                search_clear.update(cx, |input, cx| input.clear(cx));
                                state_clear.update(cx, |s, cx| {
                                    s.menu_model = None;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        bits::button("Close", Tone::Ghost)
                            .id("provider-menu-close")
                            .on_click(move |_, _, cx: &mut gpui::App| {
                                state_close.update(cx, |s, cx| {
                                    s.bot_menu_open = false;
                                    s.menu_bot = None;
                                    cx.notify();
                                });
                            }),
                    ),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> ModelOption {
        ModelOption {
            id: id.to_string(),
            label: None,
        }
    }

    #[test]
    fn search_filters_ids_case_insensitively_and_lists_all_when_empty() {
        let models = vec![
            model("gpt-5-codex"),
            model("claude-sonnet-4-5"),
            ModelOption {
                id: "o4-mini".into(),
                label: Some("Reasoning, fast".into()),
            },
        ];
        assert_eq!(filter_models("", &models).len(), 3);
        let gpt: Vec<_> = filter_models("GPT", &models)
            .iter()
            .map(|m| m.id.clone())
            .collect();
        assert_eq!(gpt, vec!["gpt-5-codex".to_string()]);
        // Labels are searchable too, not just ids.
        let by_label: Vec<_> = filter_models("reasoning", &models)
            .iter()
            .map(|m| m.id.clone())
            .collect();
        assert_eq!(by_label, vec!["o4-mini".to_string()]);
        assert!(filter_models("nope", &models).is_empty());
    }
}

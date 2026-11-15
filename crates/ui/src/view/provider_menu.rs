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
//! * both **direct** and **group** conversations can open the menu: a direct
//!   conversation targets its single bot, while a group opens the menu for a
//!   selected member bot (since group members may run on different providers
//!   and models).

use std::sync::mpsc::Sender;

use gpui::{Entity, SharedString, div, prelude::*, px, relative, rgb};
use swarmbot_client::ModelOption;

use crate::proto::Command;
use crate::state::AppState;
use crate::theme;
use crate::view::conversation::dm_bot;
use crate::view::settings::{
    backend_kind_label, connection_state, ghost_button, handshake_only_note, state_dot,
};
use crate::widgets::bits::{self, Tone};
use crate::widgets::text_input::TextInput;

/// Width of the provider column (the left one).
///
/// It was 130px, and a provider row spends that on `px_2` × 2 (16px) + a 7px
/// handshake dot + two `gap_2`s (16px) + the kind badge, whose text alone is
/// "openai-compat" (~81px at 12px) — 120px before the id gets a single
/// character. The id is the one thing this column exists to show, so the column
/// is 180px and the id cell truncates rather than being squeezed to "o…".
pub const PROVIDER_COL_W: f32 = 180.0;

/// The ring around the row of the bot's **current** backend.
///
/// A 1px line against a light pill is a UI boundary, and the vivid `SUCCESS`
/// fill is 2.4:1 there — the same invisibility the model dots had with `BORDER`.
/// The readable twin is 5.5:1 and still reads as the success hue.
pub fn current_ring() -> u32 {
    theme::SUCCESS_TEXT
}

/// The chrome a provider row puts around its id, so the id's remaining width can
/// be stated (and tested) instead of guessed: `px_2` padding (8 + 8), the 7px
/// handshake dot, and the one `gap_2` left between the dot and the id.
pub fn provider_row_chrome_px() -> f32 {
    16.0 + 7.0 + 8.0
}

/// The width the provider id keeps in a row.
///
/// The kind badge ("openai-compat" is ~81px of 12px text before its own padding)
/// used to sit *in this row*, and no honest column width fits a badge that wide,
/// a dot and an id inside a 430px card: at 180px the id kept 55px — about six
/// characters — so the badge moved to the models header, where it describes the
/// same backend and takes no width from the thing the column exists to show.
pub fn provider_id_width_px() -> f32 {
    PROVIDER_COL_W - provider_row_chrome_px()
}

/// Rough width of a 12px badge: ~0.55em per glyph plus `bits::badge`'s `px_1`
/// (4 + 4). An estimate, in the same spirit as `usage::numeric_text_width`: it
/// only has to be conservative, and [`tests`] uses it to show *why* the badge
/// could not stay in the row.
pub fn kind_badge_width_px(label: &str) -> f32 {
    const EM: f32 = 12.0;
    label.chars().count() as f32 * 0.55 * EM + 8.0
}

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
) -> Option<gpui::AnyElement> {
    if !s.bot_menu_open {
        return None;
    }
    let conversation = s.open_conversation()?;
    let bot = s
        .menu_bot
        .as_ref()
        .and_then(|id| s.bot(id).cloned())
        .or_else(|| dm_bot(s, conversation))?;
    let selected_backend = s.menu_backend.clone().unwrap_or_else(|| {
        if bot.backend == "fake"
            && s.backends
                .iter()
                .any(|b| b.id != "fake" && b.kind != swarmbot_contracts::BackendKind::Fake)
        {
            s.backends
                .iter()
                .find(|b| b.id != "fake" && b.kind != swarmbot_contracts::BackendKind::Fake)
                .map(|b| b.id.clone())
                .unwrap_or_else(|| bot.backend.clone())
        } else {
            bot.backend.clone()
        }
    });

    // --- optional bot switcher for group conversations ---------------------
    let mut bot_switcher: Option<gpui::Div> = None;
    if let swarmbot_contracts::ConversationKind::Group { members, .. } = &conversation.kind {
        let mut row = div().flex().items_center().gap_1p5().flex_wrap();
        for m_id in members {
            if let Some(member_bot) = s.bot(m_id) {
                let is_active_bot = member_bot.id == bot.id;
                let m_id_clone = member_bot.id.clone();
                let m_backend_clone = member_bot.backend.clone();
                let m_model_clone = member_bot.model.clone();
                let state_h = state.clone();
                let search_h = search.clone();
                let commands_h = commands.clone();

                let pill_label = format!("@{}", member_bot.name);
                // An inactive member's pill is `Ghost` on a white card; the
                // ringed variant, so the switcher reads as a row of choices.
                let pill = if is_active_bot {
                    bits::button(&pill_label, Tone::Primary)
                } else {
                    ghost_button(&pill_label)
                };
                row = row.child(
                    pill.id(SharedString::from(format!("bot-switch-{}", member_bot.id)))
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            cx.stop_propagation();
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
    let mut providers = div()
        .w(px(PROVIDER_COL_W))
        .flex_none()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1();
    let has_real = s
        .backends
        .iter()
        .any(|b| b.id != "fake" && b.kind != swarmbot_contracts::BackendKind::Fake);
    let visible_backends: Vec<_> = if has_real {
        s.backends
            .iter()
            .filter(|b| b.id != "fake" && b.kind != swarmbot_contracts::BackendKind::Fake)
            .collect()
    } else {
        s.backends.iter().collect()
    };
    for backend in visible_backends {
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
                .py(px(5.0))
                .rounded_lg()
                .cursor_pointer()
                .when(is_selected, |d| {
                    d.bg(rgb(0xede9fe)).text_color(rgb(0x6d28d9))
                })
                .when(!is_selected, |d| {
                    d.text_color(rgb(theme::TEXT))
                        .hover(|h| h.bg(rgb(theme::SURFACE)))
                })
                .on_click(move |_, _, cx: &mut gpui::App| {
                    cx.stop_propagation();
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
                // Two different facts on one dot: the ring colour says
                // whether this is the bot's current backend, the dot says
                // whether the last handshake passed. "Which one am I on" and
                // "which ones work" are not the same question.
                .child(bits::dot(
                    // `NotChecked` is dim, which is exactly right before any
                    // handshake has run.
                    connection_state(s.probe_for(&backend.id)).tone(),
                    px(7.0),
                ))
                .when(is_current, |d| {
                    d.border_1().border_color(rgb(current_ring()))
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_weight(if is_selected {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::NORMAL
                        })
                        .child(backend.id.clone()),
                ),
        );
    }

    // --- the selected provider's connection strip --------------------------
    //
    // The provider menu picks a provider; it can also *bring one up*, because
    // "I picked opencode and nothing answers" is the situation this panel
    // exists to fix. Same report as the Settings screen, compact, and the same
    // honesty: Connect is a handshake, never a claim that a turn works.
    let selected_probe = s.probe_for(&selected_backend);
    let selected_state = connection_state(selected_probe);
    let selected_connecting = s.is_connecting(&selected_backend);
    let connect_commands = commands.clone();
    let connect_state = state.clone();
    let connect_backend_id = selected_backend.clone();
    let mut strip = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .pt_1()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(bits::section_label("CONNECTION"))
                .child(bits::badge(
                    selected_state.label(),
                    theme::SURFACE_RAISED,
                    // The word is 12px text on a light pill: the readable twin,
                    // not the fill hue (green there is 2.1:1).
                    selected_state.text_tone(),
                ))
                .child(div().flex_1())
                .child(if selected_connecting {
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(bits::spinner(0))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child("connecting…"),
                        )
                        .into_any_element()
                } else {
                    bits::button(selected_state.connect_label(), Tone::Primary)
                        .id("provider-connect")
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            cx.stop_propagation();
                            let mut send = false;
                            let backend = connect_backend_id.clone();
                            connect_state.update(cx, |s, cx| {
                                send = s.begin_connect(&backend);
                                cx.notify();
                            });
                            if send {
                                let _ = connect_commands.send(Command::ConnectBackend {
                                    backend: connect_backend_id.clone(),
                                    method: None,
                                });
                            }
                        })
                        .into_any_element()
                }),
        )
        .children(selected_probe.and_then(|probe| {
            probe.detail.as_ref().map(|detail| {
                div()
                    .w_full()
                    .max_h(px(52.0))
                    .overflow_hidden()
                    .rounded_sm()
                    .bg(rgb(theme::CODE_BG))
                    .px_2()
                    .py_1()
                    .font_family("DejaVu Sans Mono")
                    .text_size(px(11.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(detail.clone())
            })
        }));

    // The logins that backend advertises — buttons, because the whole point is
    // that the owner should not have to read a manual to know what to do.
    if let Some(probe) = selected_probe.filter(|probe| !probe.auth_methods.is_empty()) {
        let mut logins = div().flex().flex_col().gap_1().w_full();
        for method in &probe.auth_methods {
            let commands_h = commands.clone();
            let state_h = state.clone();
            let backend_h = selected_backend.clone();
            let method_id = method.id.clone();
            logins = logins.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(match &method.description {
                                Some(description) => format!("{}: {description}", method.name),
                                None => method.name.clone(),
                            }),
                    )
                    .child(if selected_connecting {
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_DIM))
                            .child("…")
                            .into_any_element()
                    } else {
                        ghost_button("Log in")
                            .id(SharedString::from(format!("provider-login-{}", method.id)))
                            .on_click(move |_, _, cx: &mut gpui::App| {
                                cx.stop_propagation();
                                let mut send = false;
                                let backend = backend_h.clone();
                                state_h.update(cx, |s, cx| {
                                    send = s.begin_connect(&backend);
                                    cx.notify();
                                });
                                if send {
                                    let _ = commands_h.send(Command::ConnectBackend {
                                        backend: backend_h.clone(),
                                        method: Some(method_id.clone()),
                                    });
                                }
                            })
                            .into_any_element()
                    }),
            );
        }
        strip = strip.child(logins);
    }
    if let Some(note) = handshake_only_note(selected_probe) {
        // Dim, like the Settings card: the handshake passed, so this is a fact
        // about the login, not a warning about the backend.
        strip = strip.child(div().text_xs().text_color(rgb(theme::TEXT_DIM)).child(note));
    }

    // --- right column: models of the selected provider ---------------------
    let selected_backend_kind = s
        .backends
        .iter()
        .find(|backend| backend.id == selected_backend)
        .map(|backend| backend.kind);
    let query = search.read(cx).content().trim().to_string();
    let effective_model = if !query.is_empty() {
        Some(query.clone())
    } else {
        None
    };
    let listed = s.models.get(&selected_backend);
    let mut models_col = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1p5()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                // The selected backend's kind lives here now, not in the left
                // column: "openai-compat" is 81px of badge and was squeezing the
                // id it sat next to down to a couple of characters.
                .child(
                    bits::section_label(&format!("MODELS · {selected_backend}"))
                        .min_w_0()
                        .truncate(),
                )
                .children(selected_backend_kind.map(|kind| {
                    bits::badge(
                        backend_kind_label(kind),
                        theme::SURFACE_RAISED,
                        theme::TEXT_MUTED,
                    )
                })),
        )
        .child(search.clone());

    let mut list = div()
        .id("provider-model-list")
        .w_full()
        .min_w_0()
        .h(px(220.0))
        .overflow_y_scroll()
        .overflow_x_hidden()
        .flex()
        .flex_col()
        .gap_0p5();

    // Default option at the top of the model list:
    let is_default_picked = effective_model.is_none();
    let state_def = state.clone();
    let search_def = search.clone();
    let commands_def = commands.clone();
    let bot_id_def = bot.id.clone();
    let backend_def = selected_backend.clone();
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
            .py(px(5.0))
            .rounded_lg()
            .cursor_pointer()
            .when(is_default_picked, |d| {
                d.bg(rgb(0xede9fe)).text_color(rgb(0x6d28d9))
            })
            .when(!is_default_picked, |d| {
                d.text_color(rgb(theme::TEXT))
                    .hover(|h| h.bg(rgb(theme::SURFACE)))
            })
            .on_click(move |_, _, cx: &mut gpui::App| {
                cx.stop_propagation();
                search_def.update(cx, |input, cx| input.clear(cx));
                let _ = commands_def.send(Command::SetBotBackend {
                    bot: bot_id_def.clone(),
                    backend: backend_def.clone(),
                });
                let _ = commands_def.send(Command::SetBotModel {
                    bot: bot_id_def.clone(),
                    model: None,
                });
                state_def.update(cx, |s, cx| {
                    s.menu_model = None;
                    s.bot_menu_open = false;
                    s.menu_bot = None;
                    cx.notify();
                });
            })
            .child(bits::dot(state_dot(is_default_picked), px(7.0)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .font_weight(if is_default_picked {
                        gpui::FontWeight::SEMIBOLD
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .truncate()
                    .child("(Default) Backend default model"),
            ),
    );

    match listed {
        None => {
            list = list.child(
                div()
                    .py_2()
                    .text_xs()
                    .italic()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("Loading available models…"),
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
                    .py_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::DANGER_TEXT))
                            .child(format!("Could not load models: {reason}")),
                    )
                    .child(
                        ghost_button("Retry")
                            .id(SharedString::from(format!(
                                "provider-retry-{backend_retry}"
                            )))
                            .on_click(move |_, _, cx: &mut gpui::App| {
                                cx.stop_propagation();
                                let _ = commands_retry
                                    .send(Command::FetchModels(backend_retry.clone()));
                            }),
                    ),
            );
        }
        Some(Ok(mlist)) if mlist.models.is_empty() => {
            list = list.child(
                div()
                    .py_2()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("No models advertised over ACP — type a model ID above or select (Default)."),
            );
        }
        Some(Ok(mlist)) => {
            let visible = filter_models(&query, &mlist.models);
            if visible.is_empty() && !query.is_empty() {
                list = list.child(
                    div()
                        .py_2()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(format!(
                            "No model matches “{query}” — will apply as custom model ID"
                        )),
                );
            }
            for model in visible {
                let is_picked = effective_model.as_deref() == Some(model.id.as_str());
                let state_h = state.clone();
                let search_row = search.clone();
                let model_id_click = model.id.clone();
                let commands_click = commands.clone();
                let bot_id_click = bot.id.clone();
                let backend_click = selected_backend.clone();
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
                            .py(px(5.0))
                            .rounded_lg()
                            .cursor_pointer()
                            .when(is_picked, |d| d.bg(rgb(0xede9fe)).text_color(rgb(0x6d28d9)))
                            .when(!is_picked, |d| {
                                d.text_color(rgb(theme::TEXT))
                                    .hover(|h| h.bg(rgb(theme::SURFACE)))
                            })
                            .on_click(move |_, _, cx: &mut gpui::App| {
                                cx.stop_propagation();
                                search_row
                                    .update(cx, |input, cx| input.set_content(&model_id_click, cx));
                                let _ = commands_click.send(Command::SetBotBackend {
                                    bot: bot_id_click.clone(),
                                    backend: backend_click.clone(),
                                });
                                let _ = commands_click.send(Command::SetBotModel {
                                    bot: bot_id_click.clone(),
                                    model: Some(model_id_click.clone()),
                                });
                                state_h.update(cx, |s, cx| {
                                    s.menu_model = Some(model_id_click.clone());
                                    s.bot_menu_open = false;
                                    s.menu_bot = None;
                                    cx.notify();
                                });
                            })
                            .child(bits::dot(state_dot(is_picked), px(7.0)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .font_weight(if is_picked {
                                        gpui::FontWeight::SEMIBOLD
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .truncate()
                                    .child(model.id.clone()),
                            )
                            .children(model.label.as_ref().filter(|l| **l != model.id).map(
                                |label| {
                                    div()
                                        .flex_shrink_0()
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
        Some(model) => format!("{selected_backend} / {model}"),
        None => format!("{selected_backend} (default model)"),
    };
    let pick_may_not_reach = matches!(listed, Some(Ok(mlist)) if !mlist.authoritative);
    let bot_id_apply = bot.id.clone();
    let backend_apply = selected_backend.clone();
    let commands_apply = commands.clone();
    let state_apply = state.clone();
    let search_apply = search.clone();

    let state_clear = state.clone();
    let search_clear = search.clone();
    let state_close = state.clone();
    let state_backdrop = state.clone();

    Some(
        div()
            .id("provider-menu-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_click(move |_, _, cx: &mut gpui::App| {
                state_backdrop.update(cx, |s, cx| {
                    s.bot_menu_open = false;
                    s.menu_bot = None;
                    cx.notify();
                });
            })
            .child(
                div()
                    .id("provider-menu-card")
                    .occlude()
                    .absolute()
                    .top(theme::space::HEADER_H + px(4.0))
                    .left(px(16.0))
                    .w(px(430.0))
                    .max_w(relative(0.92))
                    .overflow_hidden()
                    .rounded_2xl()
                    .border_1()
                    .border_color(rgb(0xe5e7eb))
                    .bg(rgb(0xffffff))
                    .shadow_2xl()
                    .p_3p5()
                    .flex()
                    .flex_col()
                    .gap_2p5()
                    .on_click(|_, _, cx| {
                        cx.stop_propagation();
                    })
                    // Header
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
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::BOLD)
                                            .text_color(rgb(theme::TEXT))
                                            .child("Model & Provider"),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .py(px(1.0))
                                            .rounded_full()
                                            .bg(rgb(0xede9fe))
                                            .text_xs()
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(rgb(0x6d28d9))
                                            .child(format!("@{}", bot.name)),
                                    ),
                            )
                            .child(
                                div()
                                    .id("provider-menu-close")
                                    .cursor_pointer()
                                    .px_2()
                                    .py(px(2.0))
                                    .rounded_md()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .hover(|h| {
                                        h.bg(rgb(theme::SURFACE)).text_color(rgb(theme::TEXT))
                                    })
                                    .on_click(move |_, _, cx: &mut gpui::App| {
                                        cx.stop_propagation();
                                        state_close.update(cx, |s, cx| {
                                            s.bot_menu_open = false;
                                            s.menu_bot = None;
                                            cx.notify();
                                        });
                                    })
                                    .child("✕ Close"),
                            ),
                    )
                    .children(bot_switcher)
                    // Two Columns
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_3()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1p5()
                                    .child(bits::section_label("PROVIDER"))
                                    .child(providers),
                            )
                            .child(div().w(px(1.0)).bg(rgb(0xf3f4f6)))
                            .child(models_col),
                    )
                    // What it takes to make this provider answer, under the
                    // two columns: the state, the brain's own words, and the
                    // buttons (Connect, and one per login the agent advertises).
                    .child(div().w_full().h(px(1.0)).bg(rgb(0xf3f4f6)))
                    .child(strip)
                    .when(pick_may_not_reach && effective_model.is_some(), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::WARNING_TEXT))
                                .child("Note: model selection is informational on this ACP agent."),
                        )
                    })
                    .child(div().w_full().h(px(1.0)).bg(rgb(0xf3f4f6)))
                    // Footer
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
                                    .truncate()
                                    .text_xs()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(summary),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        ghost_button("Reset").id("provider-clear-model").on_click(
                                            move |_, _, cx: &mut gpui::App| {
                                                cx.stop_propagation();
                                                search_clear
                                                    .update(cx, |input, cx| input.clear(cx));
                                                state_clear.update(cx, |s, cx| {
                                                    s.menu_model = None;
                                                    cx.notify();
                                                });
                                            },
                                        ),
                                    )
                                    .child(
                                        bits::button("Apply", Tone::Primary)
                                            .id("provider-apply")
                                            .on_click(move |_, _, cx: &mut gpui::App| {
                                                cx.stop_propagation();
                                                let typed = search_apply
                                                    .read(cx)
                                                    .content()
                                                    .trim()
                                                    .to_string();
                                                let final_model =
                                                    (!typed.is_empty()).then_some(typed);
                                                let _ =
                                                    commands_apply.send(Command::SetBotBackend {
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
                                    ),
                            ),
                    ),
            )
            .into_any_element(),
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

    /// The provider column exists to show the provider's id, and at 130px with
    /// the kind badge inside the row it showed two characters of it: the badge
    /// text alone is "openai-compat", ~81px before its padding.
    #[test]
    fn the_provider_column_leaves_room_for_the_backend_id() {
        let room = provider_id_width_px();
        assert!(
            room >= 140.0,
            "the id keeps only {room:.1}px of the {PROVIDER_COL_W}px column"
        );
        // The bug, reproduced: this is the space the id had *before*, with the
        // badge still in the row. Widening the column alone would not have been
        // enough, which is why the badge moved to the models header.
        let widest_badge = [swarmbot_contracts::BackendKind::OpenAiCompat]
            .into_iter()
            .map(|kind| kind_badge_width_px(backend_kind_label(kind)))
            .fold(0.0f32, f32::max);
        assert!(
            widest_badge > 80.0,
            "the widest kind badge is {widest_badge:.1}px, not the ~81px the audit measured"
        );
        let room_with_badge_in_row = PROVIDER_COL_W - provider_row_chrome_px() - widest_badge;
        assert!(
            room_with_badge_in_row < 60.0,
            "even at {PROVIDER_COL_W}px, a row that still held the badge leaves the id \
             {room_with_badge_in_row:.1}px"
        );
        let old_130 = 130.0 - provider_row_chrome_px() - widest_badge;
        assert!(old_130 < room_with_badge_in_row);
        assert!(old_130 < 20.0, "the old row left the id {old_130:.1}px");
    }

    /// An unselected model is a state, not a gap: the dot used to be `BORDER`
    /// (`#e5e7eb`), which is 1.24:1 on white, so an unselected row had no mark at
    /// all and only the selected one showed a dot.
    #[test]
    fn an_unselected_row_still_draws_its_state_dot() {
        assert_eq!(state_dot(true), theme::SUCCESS);
        assert_eq!(state_dot(false), theme::TEXT_DIM);
        assert_ne!(state_dot(false), theme::BORDER);
        // The "current backend" ring is a 1px boundary, so it takes the readable
        // twin rather than the 2.4:1 fill hue.
        assert_eq!(current_ring(), theme::SUCCESS_TEXT);
        assert_ne!(current_ring(), theme::SUCCESS);
    }
}

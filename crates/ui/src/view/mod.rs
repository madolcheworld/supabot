//! The root window view: the top bar, the three-pane body (sidebar · main ·
//! delegation graph), and the overlays (command palette, login prompt).
//!
//! [`Root`] owns the shared [`AppState`] handle, the command sender, the two
//! focusable overlay entities ([`Composer`], [`Palette`]), the per-card
//! [`CardView`] entities, and the transcript's [`ListState`]. It re-renders when
//! the reducer notifies the state, and it is the last element on the focus path,
//! so the global actions (toggle graph/palette, escape, stop) are handled here.

pub mod cards;
pub mod composer;
pub mod conversation;
pub mod graph;
pub mod nodes;
pub mod palette;
pub mod provider_menu;
pub mod routines;
pub mod settings;
pub mod side_panel;
pub mod sidebar;

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::Duration;

use gpui::{
    App, ClickEvent, Context, Entity, FocusHandle, Focusable, ListAlignment, ListState, Render,
    SharedString, Window, div, list, prelude::*, px, rgb,
};
use swarmbot_client::NewBotRequest;
use swarmbot_contracts::{EntryId, EntryKind, FormField, RequestId};

use crate::RENDERS;
use crate::actions::{CancelOverlay, FocusComposer, StopTurn, ToggleGraph, TogglePalette};
use crate::proto::{Command, SecretToken};
use crate::state::{AppState, ConnectionState, DeleteTarget, Screen};
use crate::theme;
use crate::view::cards::CardView;
use crate::view::composer::Composer;
use crate::view::palette::Palette;
use crate::widgets::bits::{self, Tone};
use crate::widgets::text_input::TextInput;
use crate::worker::Worker;

pub struct Root {
    state: Entity<AppState>,
    commands: Sender<Command>,
    composer: Entity<Composer>,
    palette: Entity<Palette>,
    login_input: Entity<TextInput>,
    /// The model-override field inside the provider menu.
    provider_model: Entity<TextInput>,
    routine_name: Entity<TextInput>,
    routine_prompt: Entity<TextInput>,
    group_title_input: Entity<TextInput>,
    bot_handle_input: Entity<TextInput>,
    bot_label_input: Entity<TextInput>,
    bot_instructions_input: Entity<TextInput>,
    cards: HashMap<EntryId, Entity<CardView>>,
    transcript: ListState,
    /// Interior mutability so the transcript can reset its item count from a
    /// `&self` render pass (a `ListState::reset` is itself `&self`).
    transcript_count: Cell<usize>,
    focus_handle: FocusHandle,
    /// Held, not read: dropping the root must stop the worker threads.
    _worker: Option<Worker>,
    _observe: gpui::Subscription,
}

impl Root {
    pub fn new(
        state: Entity<AppState>,
        commands: Sender<Command>,
        worker: Option<Worker>,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| Composer::new(state.clone(), commands.clone(), cx));
        let palette = cx.new(|cx| Palette::new(state.clone(), commands.clone(), cx));
        let login_input = cx.new(|cx| TextInput::new("one-time login token", cx).secure());
        let provider_model = cx.new(|cx| TextInput::new("Search or enter model ID...", cx));
        let routine_name = cx.new(|cx| TextInput::new("Routine name (e.g. Daily Standup)", cx));
        let routine_prompt = cx.new(|cx| TextInput::new("Prompt for the bot…", cx).multiline());
        let group_title_input = cx.new(|cx| TextInput::new("Group title (e.g. Team Chat)", cx));
        let bot_handle_input = cx.new(|cx| TextInput::new("handle (e.g. researcher)", cx));
        let bot_label_input = cx.new(|cx| TextInput::new("Display name (e.g. Researcher)", cx));
        let bot_instructions_input =
            cx.new(|cx| TextInput::new("Instructions / role for this bot…", cx).multiline());
        let focus_handle = cx.focus_handle();
        let observe = cx.observe(&state, |this, _, cx| {
            this.sync_cards(cx);
            cx.notify();
        });
        // The transcript is a chat log: bottom-aligned, so the newest entry is
        // always in view and a streaming reply grows upward from the baseline.
        let transcript = ListState::new(0, ListAlignment::Bottom, px(200.0));
        let mut root = Self {
            state,
            commands,
            composer,
            palette,
            login_input,
            provider_model,
            routine_name,
            routine_prompt,
            group_title_input,
            bot_handle_input,
            bot_label_input,
            bot_instructions_input,
            cards: HashMap::new(),
            transcript,
            transcript_count: Cell::new(0),
            focus_handle,
            _worker: worker,
            _observe: observe,
        };
        root.sync_cards(cx);
        root
    }

    /// Ensure every card entry in the open conversation has a [`CardView`]
    /// entity (created once, so its field inputs survive re-renders).
    fn sync_cards(&mut self, cx: &mut Context<Self>) {
        let needed: Vec<(EntryId, RequestId, Vec<FormField>)> = {
            let s = self.state.read(cx);
            let mut needed = Vec::new();
            if let Some(conversation) = s.open_conversation() {
                for entry in s.entries(&conversation.id) {
                    if let EntryKind::Card(card) = &entry.kind
                        && !self.cards.contains_key(&entry.id)
                    {
                        let fields = s
                            .request(&card.request_id)
                            .map(|r| r.fields.clone())
                            .unwrap_or_default();
                        needed.push((entry.id.clone(), card.request_id.clone(), fields));
                    }
                }
            }
            needed
        };
        for (entry_id, request_id, fields) in needed {
            let state = self.state.clone();
            let commands = self.commands.clone();
            let view = cx.new(|cx| {
                CardView::new(
                    request_id.clone(),
                    entry_id.clone(),
                    &fields,
                    state.clone(),
                    commands.clone(),
                    cx,
                )
            });
            self.cards.insert(entry_id, view);
        }
    }

    pub fn focus_default(&mut self, window: &mut Window, cx: &mut App) {
        self.composer.read(cx).focus_input(window, cx);
    }

    fn toggle_graph(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.graph_open = !s.graph_open;
            cx.notify();
        });
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let opening = !self.state.read(cx).palette_open;
        self.state.update(cx, |s, cx| {
            s.palette_open = opening;
            cx.notify();
        });
        if opening {
            self.palette.read(cx).focus_input(window, cx);
        } else {
            self.composer.read(cx).focus_input(window, cx);
        }
    }

    fn on_toggle_graph(&mut self, _: &ToggleGraph, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_graph(cx);
    }

    fn on_toggle_palette(
        &mut self,
        _: &TogglePalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_palette(window, cx);
    }

    #[allow(dead_code)]
    fn on_graph_click(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_graph(cx);
    }

    #[allow(dead_code)]
    fn on_palette_click(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_palette(window, cx);
    }

    fn on_cancel(&mut self, _: &CancelOverlay, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.read(cx).confirm_delete.is_some() {
            self.state.update(cx, |s, cx| {
                s.confirm_delete = None;
                cx.notify();
            });
            return;
        }
        if self.state.read(cx).create_bot_open {
            self.state.update(cx, |s, cx| {
                s.create_bot_open = false;
                cx.notify();
            });
            return;
        }
        if self.state.read(cx).create_group_open {
            self.state.update(cx, |s, cx| {
                s.create_group_open = false;
                cx.notify();
            });
            return;
        }
        if self.state.read(cx).bot_menu_open {
            self.state.update(cx, |s, cx| {
                s.bot_menu_open = false;
                cx.notify();
            });
            return;
        }
        if self.state.read(cx).palette_open {
            self.state.update(cx, |s, cx| {
                s.palette_open = false;
                cx.notify();
            });
            self.composer.read(cx).focus_input(window, cx);
            return;
        }
        // Otherwise let the composer close its mention picker.
        self.composer.update(cx, |composer, cx| {
            composer.close_picker(cx);
        });
    }

    fn on_stop(&mut self, _: &StopTurn, _: &mut Window, cx: &mut Context<Self>) {
        let conversation = self.state.read(cx).open_conversation.clone();
        if let Some(conversation) = conversation {
            let _ = self.commands.send(Command::Interrupt(conversation));
        }
    }

    fn on_focus_composer(
        &mut self,
        _: &FocusComposer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer.read(cx).focus_input(window, cx);
    }

    fn on_submit_login(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let token = self.login_input.read(cx).content().to_string();
        if token.trim().is_empty() {
            return;
        }
        let _ = self.commands.send(Command::Login(SecretToken::new(token)));
        self.login_input.update(cx, |input, cx| input.clear(cx));
    }

    fn render_conversation(&self, s: &AppState, cx: &gpui::App) -> gpui::Div {
        let count = s.open_entries().len();
        if count != self.transcript_count.get() {
            self.transcript.reset(count);
            self.transcript_count.set(count);
        }
        let state_h = self.state.clone();
        let commands_h = self.commands.clone();
        let cards = self.cards.clone();
        let transcript = list(self.transcript.clone(), move |ix, _window, cx: &mut App| {
            let s = state_h.read(cx);
            let entries = s.open_entries();
            match entries.get(ix) {
                Some(entry) => {
                    // The previous entry drives message grouping (same author,
                    // close in time → one visual block).
                    let prev = if ix > 0 { entries.get(ix - 1) } else { None };
                    conversation::entry_row(
                        s,
                        &state_h,
                        &commands_h,
                        &cards,
                        entry,
                        prev,
                        ix + 1 == entries.len(),
                    )
                }
                None => div().into_any_element(),
            }
        })
        .w_full()
        .h_full();

        let transcript_area = if s.open_conversation().is_none() {
            let state_h = self.state.clone();
            let commands_h = self.commands.clone();
            let headline = if s.bots.is_empty() {
                "No bots in your team yet"
            } else {
                "No conversation selected"
            };
            let hint = if s.bots.is_empty() {
                "Add a bot to start chatting, or add 2+ bots to form a group."
            } else {
                "Select a bot or group from the left sidebar, or add a new bot."
            };
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    div()
                        .text_base()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(headline),
                )
                .child(div().text_sm().text_color(rgb(theme::TEXT_DIM)).child(hint))
                .child(
                    div()
                        .id("empty-add-bot-btn")
                        .px_4()
                        .py_2()
                        .rounded_lg()
                        .bg(rgb(theme::ACCENT))
                        .hover(|h| h.bg(rgb(theme::ACCENT_HOVER)))
                        .cursor_pointer()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::BACKGROUND))
                        .child("+ Add Bot")
                        .on_click(move |_, _, cx: &mut App| {
                            state_h.update(cx, |st, cx| {
                                st.create_bot_open = true;
                                st.create_group_open = false;
                                cx.notify();
                            });
                            let _ = commands_h.send(Command::FetchBackends);
                        }),
                )
                .into_any_element()
        } else if count == 0 {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("No messages yet"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child("Send a message below to start chatting."),
                )
                .into_any_element()
        } else {
            transcript.into_any_element()
        };

        div()
            .relative()
            .flex_1()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(conversation::header(
                s,
                &self.state,
                &self.provider_model,
                &self.commands,
            ))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .px_4()
                    .py_2()
                    .child(transcript_area),
            )
            .child(self.composer.clone())
            // Last child = painted last = above the transcript it floats over.
            .children(provider_menu::render(
                s,
                &self.state,
                &self.provider_model,
                &self.commands,
                cx,
            ))
    }

    fn render_palette_overlay(&self) -> gpui::Div {
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(72.0))
            .bg(gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.35,
            })
            .child(self.palette.clone())
    }

    fn render_login(
        &self,
        s: &AppState,
        on_submit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> gpui::Div {
        let reason = s
            .login_error
            .clone()
            .unwrap_or_else(|| "the brain needs a login token".to_string());
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.5,
            })
            .child(
                div()
                    .w(px(420.0))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(theme::ACCENT))
                    .bg(rgb(theme::SURFACE))
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_base()
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("Login required"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(reason),
                    )
                    .child(self.login_input.clone())
                    .child(
                        div().flex().justify_end().child(
                            bits::button("Log in", Tone::Primary)
                                .id("login-submit")
                                .on_click(on_submit),
                        ),
                    ),
            )
    }

    fn render_create_tabs(&self, active_is_bot: bool) -> gpui::Div {
        let state_bot = self.state.clone();
        let commands_bot = self.commands.clone();
        let state_group = self.state.clone();

        div()
            .flex()
            .items_center()
            .gap_1()
            .p_1()
            .rounded_xl()
            .bg(rgb(theme::SURFACE_RAISED))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .id("modal-tab-add-bot")
                    .flex_1()
                    .py_1p5()
                    .rounded_lg()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .when(active_is_bot, |d| {
                        d.bg(rgb(theme::ACCENT)).text_color(rgb(theme::BACKGROUND))
                    })
                    .when(!active_is_bot, |d| {
                        d.cursor_pointer()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .hover(|h| h.bg(rgb(0xebebee)).text_color(rgb(theme::TEXT)))
                            .on_click(move |_, _, cx: &mut App| {
                                state_bot.update(cx, |st, cx| {
                                    st.create_bot_open = true;
                                    st.create_group_open = false;
                                    cx.notify();
                                });
                                let _ = commands_bot.send(Command::FetchBackends);
                            })
                    })
                    .child("+ Add Bot"),
            )
            .child(
                div()
                    .id("modal-tab-create-group")
                    .flex_1()
                    .py_1p5()
                    .rounded_lg()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .when(!active_is_bot, |d| {
                        d.bg(rgb(theme::ACCENT)).text_color(rgb(theme::BACKGROUND))
                    })
                    .when(active_is_bot, |d| {
                        d.cursor_pointer()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .hover(|h| h.bg(rgb(0xebebee)).text_color(rgb(theme::TEXT)))
                            .on_click(move |_, _, cx: &mut App| {
                                state_group.update(cx, |st, cx| {
                                    st.create_group_open = true;
                                    st.create_bot_open = false;
                                    cx.notify();
                                });
                            })
                    })
                    .child("Create Group"),
            )
    }

    fn render_create_group(&self, s: &AppState) -> gpui::Div {
        let state_h = self.state.clone();
        let commands = self.commands.clone();
        let title_input = self.group_title_input.clone();
        let selected_bots = s.create_group_members.clone();
        let count = selected_bots.len();
        let can_create = (swarmbot_contracts::MIN_GROUP_MEMBERS
            ..=swarmbot_contracts::MAX_GROUP_MEMBERS)
            .contains(&count);

        let on_close = {
            let state_h = state_h.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                state_h.update(cx, |st, cx| {
                    st.create_group_open = false;
                    cx.notify();
                });
            }
        };

        let on_create = {
            let state_h = state_h.clone();
            let commands = commands.clone();
            let title_input = title_input.clone();
            let selected_bots = selected_bots.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let typed = title_input.read(cx).content().trim().to_string();
                let title = if typed.is_empty() {
                    "Team Chat".to_string()
                } else {
                    typed
                };
                let lead = selected_bots.first().cloned();
                let _ = commands.send(Command::CreateGroup {
                    title,
                    members: selected_bots.clone(),
                    lead,
                });
                state_h.update(cx, |st, cx| {
                    st.create_group_open = false;
                    cx.notify();
                });
            }
        };

        let mut bot_list = div()
            .id("create-group-bot-list")
            .flex_1()
            .max_h(px(220.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1();

        for bot in &s.bots {
            let bot_id = bot.id.clone();
            let checked = selected_bots.contains(&bot_id);
            let state_h = state_h.clone();
            let bot_id_c = bot_id.clone();
            bot_list = bot_list.child(
                div()
                    .id(SharedString::from(format!("create-group-bot-{}", bot.id)))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .rounded_xl()
                    .cursor_pointer()
                    .bg(if checked {
                        rgb(theme::SURFACE_RAISED)
                    } else {
                        rgb(theme::SURFACE)
                    })
                    .hover(|h| h.bg(rgb(0xebebee)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(bits::avatar(
                                theme::bot_color(bot),
                                bits::initial(&bot.label),
                                px(26.0),
                            ))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(theme::TEXT))
                                    .child(bot.label.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::TEXT_DIM))
                                    .child(format!("@{}", bot.name)),
                            ),
                    )
                    .child(div().text_sm().child(if checked { "✓" } else { "○" }))
                    .on_click(move |_, _, cx: &mut App| {
                        let bid = bot_id_c.clone();
                        state_h.update(cx, |st, cx| {
                            if let Some(pos) =
                                st.create_group_members.iter().position(|id| id == &bid)
                            {
                                st.create_group_members.remove(pos);
                            } else if st.create_group_members.len()
                                < swarmbot_contracts::MAX_GROUP_MEMBERS
                            {
                                st.create_group_members.push(bid);
                            }
                            cx.notify();
                        });
                    }),
            );
        }

        if s.bots.len() < swarmbot_contracts::MIN_GROUP_MEMBERS {
            let state_go = self.state.clone();
            let commands_go = self.commands.clone();
            let msg = if s.bots.is_empty() {
                "No bots added yet. Add at least 2 bots to form a group."
            } else {
                "Add 1 more bot to form a group chat (minimum 2)."
            };
            bot_list = bot_list.child(
                div()
                    .p_3()
                    .rounded_xl()
                    .bg(rgb(theme::SURFACE_RAISED))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(msg),
                    )
                    .child(
                        div()
                            .id("group-empty-go-add-bot")
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
                            .child("+ Add Bot First")
                            .on_click(move |_, _, cx: &mut App| {
                                state_go.update(cx, |st, cx| {
                                    st.create_group_open = false;
                                    st.create_bot_open = true;
                                    cx.notify();
                                });
                                let _ = commands_go.send(Command::FetchBackends);
                            }),
                    ),
            );
        }

        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.5,
            })
            .child(
                div()
                    .w(px(460.0))
                    .max_h(px(540.0))
                    .rounded_2xl()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::SURFACE))
                    .shadow_lg()
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(theme::TEXT))
                                    .child("Create Group Chat"),
                            )
                            .child(
                                div()
                                    .id("create-group-close")
                                    .cursor_pointer()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .hover(|h| h.text_color(rgb(theme::TEXT)))
                                    .child("✕")
                                    .on_click(on_close.clone()),
                            ),
                    )
                    .child(self.render_create_tabs(false))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(format!(
                                "Select {}-{} bots to collaborate in this room.",
                                swarmbot_contracts::MIN_GROUP_MEMBERS,
                                swarmbot_contracts::MAX_GROUP_MEMBERS
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(theme::TEXT_DIM))
                                    .child("GROUP TITLE"),
                            )
                            .child(self.group_title_input.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(theme::TEXT_DIM))
                            .child(format!("SELECT MEMBERS ({count}/6)")),
                    )
                    .child(bot_list)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .border_t_1()
                            .border_color(rgb(theme::BORDER))
                            .child(
                                div()
                                    .id("create-group-cancel")
                                    .px_3()
                                    .py_1p5()
                                    .rounded_lg()
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .hover(|h| h.text_color(rgb(theme::TEXT)))
                                    .child("Cancel")
                                    .on_click(on_close),
                            )
                            .child(
                                div()
                                    .id("create-group-submit")
                                    .px_4()
                                    .py_1p5()
                                    .rounded_lg()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .when(can_create, |d| {
                                        d.cursor_pointer()
                                            .bg(rgb(theme::ACCENT))
                                            .text_color(rgb(theme::BACKGROUND))
                                            .on_click(on_create)
                                    })
                                    .when(!can_create, |d| {
                                        d.bg(rgb(0xd1d5db)).text_color(rgb(theme::TEXT_DIM))
                                    })
                                    .child("Create Group"),
                            ),
                    ),
            )
    }

    fn render_create_bot(&self, s: &AppState, cx: &App) -> gpui::Div {
        let state_close = self.state.clone();
        let on_close = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            state_close.update(cx, |st, cx| {
                st.create_bot_open = false;
                cx.notify();
            });
        };

        let state_create = self.state.clone();
        let commands = self.commands.clone();
        let handle_input = self.bot_handle_input.clone();
        let label_input = self.bot_label_input.clone();
        let instructions_input = self.bot_instructions_input.clone();
        let bot_count = s.bots.len();

        let selected_backend = s
            .create_bot_backend
            .as_ref()
            .filter(|id| s.backends.iter().any(|b| &b.id == *id))
            .or_else(|| s.backends.first().map(|b| &b.id))
            .or_else(|| s.bots.first().map(|b| &b.backend))
            .cloned()
            .unwrap_or_else(|| "opencode".to_string());

        let live_handle = {
            let typed = normalize_bot_handle(handle_input.read(cx).content());
            if typed.is_empty() {
                normalize_bot_handle(label_input.read(cx).content())
            } else {
                typed
            }
        };
        let can_create = !live_handle.is_empty();

        let chosen_backend = selected_backend.clone();
        let on_create = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let handle_typed = normalize_bot_handle(handle_input.read(cx).content());
            let label_raw = label_input.read(cx).content().trim().to_string();
            let handle = if handle_typed.is_empty() {
                normalize_bot_handle(&label_raw)
            } else {
                handle_typed
            };
            let instructions = instructions_input.read(cx).content().trim().to_string();

            if handle.is_empty() {
                return;
            }
            let label = if label_raw.is_empty() {
                handle.clone()
            } else {
                label_raw
            };

            let colors = [
                "#8b5cf6", "#f97316", "#ef4444", "#ec4899", "#06b6d4", "#10b981", "#a855f7",
                "#3b82f6",
            ];
            let color = colors[bot_count % colors.len()];

            let mut req = NewBotRequest::new(handle, label, instructions, chosen_backend.clone());
            req.color = Some(color.to_string());

            let _ = commands.send(Command::CreateBot(req));
            handle_input.update(cx, |input, cx| input.set_content("", cx));
            label_input.update(cx, |input, cx| input.set_content("", cx));
            instructions_input.update(cx, |input, cx| input.set_content("", cx));

            state_create.update(cx, |st, cx| {
                st.create_bot_open = false;
                cx.notify();
            });
        };

        let mut backend_row = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_1p5()
            .text_xs()
            .text_color(rgb(theme::TEXT_DIM))
            .child("Backend:");

        if s.backends.is_empty() {
            backend_row = backend_row.child(
                div()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(rgb(theme::SURFACE_RAISED))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(selected_backend),
            );
        } else {
            for backend in &s.backends {
                let bid = backend.id.clone();
                let is_selected = bid == selected_backend;
                let state_b = self.state.clone();
                let bid_click = bid.clone();
                backend_row = backend_row.child(
                    div()
                        .id(SharedString::from(format!("create-bot-backend-{bid}")))
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .when(is_selected, |d| {
                            d.bg(rgb(theme::ACCENT)).text_color(rgb(theme::BACKGROUND))
                        })
                        .when(!is_selected, |d| {
                            d.cursor_pointer()
                                .bg(rgb(theme::SURFACE_RAISED))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .hover(|h| h.bg(rgb(0xebebee)).text_color(rgb(theme::TEXT)))
                                .on_click(move |_, _, cx: &mut App| {
                                    let chosen = bid_click.clone();
                                    state_b.update(cx, |st, cx| {
                                        st.create_bot_backend = Some(chosen);
                                        cx.notify();
                                    });
                                })
                        })
                        .child(bid),
                );
            }
        }

        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.5,
            })
            .child(
                div()
                    .w(px(460.0))
                    .rounded_2xl()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::SURFACE))
                    .shadow_lg()
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(theme::TEXT))
                                    .child("Add Bot to Team"),
                            )
                            .child(
                                div()
                                    .id("create-bot-close")
                                    .cursor_pointer()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .hover(|h| h.text_color(rgb(theme::TEXT)))
                                    .child("✕")
                                    .on_click(on_close.clone()),
                            ),
                    )
                    .child(self.render_create_tabs(true))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child("Configure a new autonomous teammate for your workspace."),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(theme::TEXT_DIM))
                                    .child("BOT HANDLE (@NAME, OPTIONAL)"),
                            )
                            .child(self.bot_handle_input.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(theme::TEXT_DIM))
                                    .child("DISPLAY NAME"),
                            )
                            .child(self.bot_label_input.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(theme::TEXT_DIM))
                                    .child("SYSTEM PROMPT / ROLE"),
                            )
                            .child(self.bot_instructions_input.clone()),
                    )
                    .child(backend_row)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .border_t_1()
                            .border_color(rgb(theme::BORDER))
                            .child(
                                div()
                                    .id("create-bot-cancel")
                                    .px_3()
                                    .py_1p5()
                                    .rounded_lg()
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .hover(|h| h.text_color(rgb(theme::TEXT)))
                                    .child("Cancel")
                                    .on_click(on_close),
                            )
                            .child(
                                div()
                                    .id("create-bot-submit")
                                    .px_4()
                                    .py_1p5()
                                    .rounded_lg()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .when(can_create, |d| {
                                        d.cursor_pointer()
                                            .bg(rgb(theme::ACCENT))
                                            .text_color(rgb(theme::BACKGROUND))
                                            .on_click(on_create)
                                    })
                                    .when(!can_create, |d| {
                                        d.bg(rgb(0xd1d5db)).text_color(rgb(theme::TEXT_DIM))
                                    })
                                    .child("Add Bot"),
                            ),
                    ),
            )
    }

    fn render_confirm_delete(&self, s: &AppState) -> gpui::Div {
        let Some(target) = &s.confirm_delete else {
            return div();
        };

        let state_close = self.state.clone();
        let on_close = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            state_close.update(cx, |st, cx| {
                st.confirm_delete = None;
                cx.notify();
            });
        };

        let state_confirm = self.state.clone();
        let commands = self.commands.clone();
        let target_clone = target.clone();

        let (title, message, submit_label) = match &target {
            DeleteTarget::Bot { label, .. } => (
                "Delete Bot".to_string(),
                format!(
                    "Are you sure you want to delete bot \"{label}\"? Its direct messages and state will be removed."
                ),
                "Delete Bot".to_string(),
            ),
            DeleteTarget::Group { title, .. } => (
                "Delete Group".to_string(),
                format!(
                    "Are you sure you want to delete group \"{title}\"? All conversation messages will be removed."
                ),
                "Delete Group".to_string(),
            ),
        };

        let on_confirm = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            match &target_clone {
                DeleteTarget::Bot { id, .. } => {
                    let _ = commands.send(Command::DeleteBot(id.clone()));
                }
                DeleteTarget::Group { id, .. } => {
                    let _ = commands.send(Command::DeleteGroup(id.clone()));
                }
            }
            state_confirm.update(cx, |st, cx| {
                st.confirm_delete = None;
                cx.notify();
            });
        };

        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.5,
            })
            .child(
                div()
                    .w(px(400.0))
                    .rounded_2xl()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::SURFACE))
                    .shadow_lg()
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(theme::DANGER))
                                    .child(title),
                            )
                            .child(
                                div()
                                    .id("confirm-delete-close")
                                    .cursor_pointer()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .hover(|h| h.text_color(rgb(theme::TEXT)))
                                    .child("✕")
                                    .on_click(on_close.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(message),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .border_t_1()
                            .border_color(rgb(theme::BORDER))
                            .child(
                                div()
                                    .id("confirm-delete-cancel")
                                    .px_3()
                                    .py_1p5()
                                    .rounded_lg()
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .hover(|h| h.text_color(rgb(theme::TEXT)))
                                    .child("Cancel")
                                    .on_click(on_close),
                            )
                            .child(
                                div()
                                    .id("confirm-delete-submit")
                                    .px_4()
                                    .py_1p5()
                                    .rounded_lg()
                                    .cursor_pointer()
                                    .bg(rgb(theme::DANGER))
                                    .text_color(rgb(0xffffff))
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(submit_label)
                                    .on_click(on_confirm),
                            ),
                    ),
            )
    }
}

impl Focusable for Root {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        RENDERS.fetch_add(1, Ordering::SeqCst);
        let s = self.state.read(cx);

        let main = match s.screen {
            Screen::Conversation => self.render_conversation(s, cx),
            Screen::Nodes => nodes::render(s, &self.state, &self.commands),
            Screen::Routines => routines::render(
                s,
                &self.state,
                &self.commands,
                &self.routine_name,
                &self.routine_prompt,
            ),
            Screen::Settings => settings::render(s, &self.state, &self.commands),
        };

        let mut root = div()
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .overflow_hidden()
            .bg(rgb(theme::BACKGROUND))
            .text_color(rgb(theme::TEXT))
            .key_context("Root")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::on_toggle_graph))
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_stop))
            .on_action(cx.listener(Self::on_focus_composer))
            .child(sidebar::render(s, &self.state, &self.commands))
            .child(main)
            .when(s.graph_open, |d| {
                d.child(side_panel::render(
                    s,
                    &self.state,
                    &self.commands,
                    &self.routine_name,
                    &self.routine_prompt,
                ))
            });

        if s.needs_login {
            root = root.child(self.render_login(s, cx.listener(Self::on_submit_login)));
        }
        if s.palette_open {
            root = root.child(self.render_palette_overlay());
        }
        if s.create_group_open {
            root = root.child(self.render_create_group(s));
        }
        if s.create_bot_open {
            root = root.child(self.render_create_bot(s, cx));
        }
        if s.confirm_delete.is_some() {
            root = root.child(self.render_confirm_delete(s));
        }
        root
    }
}

fn normalize_bot_handle(raw: &str) -> String {
    raw.trim()
        .to_lowercase()
        .trim_start_matches('@')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[allow(dead_code)]
fn render_top_bar(
    s: &AppState,
    on_palette: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_graph: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let resynced = s
        .resynced_at
        .is_some_and(|at| at.elapsed() < Duration::from_secs(4));
    let (label, color) = if resynced {
        ("resynced".to_string(), theme::SUCCESS)
    } else {
        match &s.conn {
            ConnectionState::Connected => ("connected".to_string(), theme::SUCCESS),
            ConnectionState::Connecting => ("connecting…".to_string(), theme::WARNING),
            ConnectionState::Reconnecting { attempt } => {
                (format!("reconnecting (attempt {attempt})"), theme::WARNING)
            }
            ConnectionState::Offline { .. } => (s.conn.label(), theme::DANGER),
        }
    };

    div()
        .h(theme::space::HEADER_H)
        .flex_none()
        .flex()
        .items_center()
        .gap_3()
        .px_4()
        .border_b_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::TEXT))
                .child("SwarmBot"),
        )
        .child(bits::dot(color, px(9.0)))
        .child(div().text_xs().text_color(rgb(color)).child(label))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .child(s.brain_url.clone()),
        )
        .children(s.toast.as_ref().map(|toast| {
            div()
                .text_xs()
                .text_color(rgb(theme::ACCENT_HOVER))
                .child(toast.clone())
        }))
        .child(div().flex_1())
        .child(
            bits::button(
                if s.graph_open { "Hide graph" } else { "Graph" },
                Tone::Ghost,
            )
            .id("toggle-graph")
            .on_click(on_graph),
        )
        .child(
            bits::button("Palette", Tone::Ghost)
                .id("toggle-palette")
                .on_click(on_palette),
        )
}

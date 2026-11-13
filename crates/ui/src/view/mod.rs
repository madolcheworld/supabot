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
pub mod usage;

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use gpui::{
    App, ClickEvent, Context, Entity, FocusHandle, Focusable, ListAlignment, ListState, Render,
    SharedString, Window, div, list, prelude::*, px, rgb,
};
use swarmbot_client::NewBotRequest;
use swarmbot_contracts::{BotId, ConversationKind, EntryId, EntryKind, FormField, RequestId};

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
    edit_group_title_input: Entity<TextInput>,
    bot_handle_input: Entity<TextInput>,
    bot_label_input: Entity<TextInput>,
    bot_instructions_input: Entity<TextInput>,
    cards: HashMap<EntryId, Entity<CardView>>,
    /// Pending approval/question cards surfaced into a group view from
    /// delegated DM `handoff_task` turns (whose entries live in a bot's DM).
    surfaced_cards: HashMap<RequestId, Entity<CardView>>,
    transcript: ListState,
    /// Interior mutability so the transcript can reset its item count from a
    /// `&self` render pass (a `ListState::reset` is itself `&self`).
    transcript_count: Cell<usize>,
    focus_handle: FocusHandle,
    /// Held, not read: dropping the root must stop the worker threads.
    _worker: Option<Worker>,
    _observations: Vec<gpui::Subscription>,
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
        let group_title_input = cx.new(|cx| TextInput::new("Group title (e.g. Group Chat)", cx));
        let edit_group_title_input = cx.new(|cx| TextInput::new("Group title", cx));
        let bot_handle_input = cx.new(|cx| TextInput::new("handle (e.g. researcher)", cx));
        let bot_label_input = cx.new(|cx| TextInput::new("Display name (e.g. Researcher)", cx));
        let bot_instructions_input =
            cx.new(|cx| TextInput::new("Instructions / role for this bot…", cx).multiline());
        let focus_handle = cx.focus_handle();
        let observations = vec![
            cx.observe(&state, |this, state, cx| {
                if let Some(seed) = state.read(cx).edit_group_title_seed.clone() {
                    state.update(cx, |s, _| {
                        s.edit_group_title_seed = None;
                    });
                    this.edit_group_title_input
                        .update(cx, |input, cx| input.set_content(&seed, cx));
                }
                this.sync_cards(cx);
                cx.notify();
            }),
            // Re-render Root when the user types into the Add Bot fields so
            // submit availability updates immediately even while idle.
            cx.observe(&bot_handle_input, |_, _, cx| cx.notify()),
            cx.observe(&bot_label_input, |_, _, cx| cx.notify()),
        ];
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
            edit_group_title_input,
            bot_handle_input,
            bot_label_input,
            bot_instructions_input,
            cards: HashMap::new(),
            surfaced_cards: HashMap::new(),
            transcript,
            transcript_count: Cell::new(0),
            focus_handle,
            _worker: worker,
            _observations: observations,
        };
        root.sync_cards(cx);
        root
    }

    /// Ensure every card entry in the open conversation (and any pending card
    /// surfaced from a group's delegated tasks) has a [`CardView`] entity
    /// created once so its field inputs survive re-renders.
    fn sync_cards(&mut self, cx: &mut Context<Self>) {
        type CardNeed = (EntryId, RequestId, Vec<FormField>);
        let (needed, surfaced_needed, still_pending): (
            Vec<CardNeed>,
            Vec<CardNeed>,
            Vec<RequestId>,
        ) = {
            let s = self.state.read(cx);
            let mut needed = Vec::new();
            let mut surfaced_needed = Vec::new();
            let mut still_pending = Vec::new();
            if let Some(conversation) = s.open_conversation() {
                let mut inline_req_ids = Vec::new();
                for entry in s.entries(&conversation.id) {
                    if let EntryKind::Card(card) = &entry.kind {
                        inline_req_ids.push(card.request_id.clone());
                        if !self.cards.contains_key(&entry.id) {
                            let fields = s
                                .request(&card.request_id)
                                .map(|r| r.fields.clone())
                                .unwrap_or_default();
                            needed.push((entry.id.clone(), card.request_id.clone(), fields));
                        }
                    }
                }
                if conversation.kind.is_group() {
                    for req in s.group_pending_requests(conversation) {
                        if !inline_req_ids.contains(&req.id) {
                            still_pending.push(req.id.clone());
                            if !self.surfaced_cards.contains_key(&req.id) {
                                let eid = req.card_entry_id.clone().unwrap_or_else(|| {
                                    EntryId::from_suffix(&format!("surfaced-{}", req.id))
                                });
                                surfaced_needed.push((eid, req.id.clone(), req.fields.clone()));
                            }
                        }
                    }
                }
            }
            (needed, surfaced_needed, still_pending)
        };
        self.surfaced_cards
            .retain(|id, _| still_pending.contains(id));
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
        for (entry_id, request_id, fields) in surfaced_needed {
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
            self.surfaced_cards.insert(request_id, view);
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
            if opening {
                s.plus_menu_open = false;
            }
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

    fn on_graph_click(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_graph(cx);
    }

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
            self.composer.read(cx).focus_input(window, cx);
            return;
        }
        if self.state.read(cx).create_group_open {
            self.state.update(cx, |s, cx| {
                s.create_group_open = false;
                cx.notify();
            });
            self.composer.read(cx).focus_input(window, cx);
            return;
        }
        if self.state.read(cx).edit_group_id.is_some() {
            self.state.update(cx, |s, cx| {
                s.edit_group_id = None;
                s.edit_group_title_seed = None;
                cx.notify();
            });
            self.composer.read(cx).focus_input(window, cx);
            return;
        }
        if self.state.read(cx).bot_menu_open {
            self.state.update(cx, |s, cx| {
                s.bot_menu_open = false;
                cx.notify();
            });
            return;
        }
        if self.state.read(cx).plus_menu_open {
            self.state.update(cx, |s, cx| {
                s.plus_menu_open = false;
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
        // Otherwise let the composer close its mention picker; if none was open
        // and a turn is running in the open conversation, Escape stops it.
        let had_picker = self
            .composer
            .update(cx, |composer, cx| composer.close_picker(cx));
        if !had_picker {
            let running_conv = {
                let s = self.state.read(cx);
                s.open_conversation
                    .as_ref()
                    .filter(|id| s.running_turn(id).is_some())
                    .cloned()
            };
            if let Some(conv) = running_conv {
                let _ = self.commands.send(Command::Interrupt(conv));
            }
        }
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
            let handle_input = self.bot_handle_input.clone();
            let headline = if s.bots.is_empty() {
                "No bots yet"
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
                        .on_click(move |_, window, cx: &mut App| {
                            state_h.update(cx, |st, cx| {
                                st.open_create_bot();
                                cx.notify();
                            });
                            handle_input.read(cx).focus(window);
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

        let surfaced_pending: Vec<Entity<CardView>> = s
            .open_conversation()
            .filter(|c| c.kind.is_group())
            .map(|conv| {
                let inline_req_ids: Vec<&RequestId> = s
                    .entries(&conv.id)
                    .iter()
                    .filter_map(|e| match &e.kind {
                        EntryKind::Card(card) => Some(&card.request_id),
                        _ => None,
                    })
                    .collect();
                s.group_pending_requests(conv)
                    .into_iter()
                    .filter(|r| !inline_req_ids.contains(&&r.id))
                    .filter_map(|r| self.surfaced_cards.get(&r.id).cloned())
                    .collect()
            })
            .unwrap_or_default();

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
            .when(!surfaced_pending.is_empty(), |d| {
                d.child(
                    div()
                        .id("group-surfaced-cards")
                        .w_full()
                        .max_h(px(260.0))
                        .overflow_y_scroll()
                        .px_4()
                        .py_2()
                        .border_t_1()
                        .border_color(rgb(theme::BORDER))
                        .bg(rgb(theme::SURFACE))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(bits::section_label("PENDING APPROVALS & QUESTIONS"))
                        .children(surfaced_pending),
                )
            })
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

    fn render_plus_menu(&self, s: &AppState) -> gpui::Stateful<gpui::Div> {
        let state_backdrop = self.state.clone();
        let state_bot = self.state.clone();
        let commands_bot = self.commands.clone();
        let handle_input = self.bot_handle_input.clone();
        let state_group = self.state.clone();
        let group_title_input = self.group_title_input.clone();

        let group_subtitle = if s.bots.len() < swarmbot_contracts::MIN_GROUP_MEMBERS {
            format!(
                "Requires at least {} bots",
                swarmbot_contracts::MIN_GROUP_MEMBERS
            )
        } else {
            "Start a multi-bot chat".to_string()
        };

        div()
            .id("plus-menu-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_click(move |_, _, cx: &mut App| {
                state_backdrop.update(cx, |st, cx| {
                    st.plus_menu_open = false;
                    cx.notify();
                });
            })
            .child(
                div()
                    .id("plus-menu-card")
                    .occlude()
                    .absolute()
                    .top(px(54.0))
                    // Right-align with the `+` button at the top-right of the left sidebar.
                    .left(theme::space::SIDEBAR_W - px(12.0) - px(220.0))
                    .w(px(220.0))
                    .rounded_xl()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::BACKGROUND))
                    .shadow_lg()
                    .p_1p5()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .on_click(|_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(plus_menu_item(
                        "plus-menu-add-bot",
                        "+",
                        0xeff6ff,
                        theme::ACCENT,
                        "Add Bot",
                        "Create a standalone bot",
                        move |window, cx| {
                            state_bot.update(cx, |st, cx| {
                                st.open_create_bot();
                                cx.notify();
                            });
                            handle_input.read(cx).focus(window);
                            let _ = commands_bot.send(Command::FetchBackends);
                        },
                    ))
                    .child(plus_menu_item(
                        "plus-menu-create-group",
                        "#",
                        0xf5f3ff,
                        0x7c3aed,
                        "Create Group",
                        &group_subtitle,
                        move |window, cx| {
                            group_title_input.update(cx, |input, cx| input.clear(cx));
                            state_group.update(cx, |st, cx| {
                                st.open_create_group();
                                cx.notify();
                            });
                            group_title_input.read(cx).focus(window);
                        },
                    )),
            )
    }

    fn render_create_group(&self, s: &AppState) -> gpui::Stateful<gpui::Div> {
        let selected_bots = s.create_group_members.clone();
        let active_lead = s
            .create_group_lead
            .clone()
            .filter(|id| selected_bots.contains(id))
            .or_else(|| selected_bots.first().cloned());
        let state_h = self.state.clone();
        let commands = self.commands.clone();
        let title_input = self.group_title_input.clone();
        let composer_create = self.composer.clone();
        let composer_close = self.composer.clone();

        let on_close = {
            let state_h = state_h.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                state_h.update(cx, |st, cx| {
                    st.create_group_open = false;
                    cx.notify();
                });
                composer_close.read(cx).focus_input(window, cx);
            }
        };

        let on_submit = {
            let state_h = state_h.clone();
            let title_input = title_input.clone();
            let selected_bots = selected_bots.clone();
            let active_lead = active_lead.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let typed = title_input.read(cx).content().trim().to_string();
                let title = if typed.is_empty() {
                    "Group Chat".to_string()
                } else {
                    typed
                };
                let _ = commands.send(Command::CreateGroup {
                    title,
                    members: selected_bots.clone(),
                    lead: active_lead.clone(),
                });
                title_input.update(cx, |input, cx| input.set_content("", cx));
                state_h.update(cx, |st, cx| {
                    st.create_group_open = false;
                    cx.notify();
                });
                composer_create.read(cx).focus_input(window, cx);
            }
        };

        self.render_group_modal(
            s,
            GroupModalConfig {
                id_prefix: "create-group",
                heading: "Create Group",
                submit_label: "Create Group",
                title_input: &self.group_title_input,
                selected_bots: &selected_bots,
                active_lead: active_lead.as_ref(),
                on_toggle_member: Box::new(move |bid, st| {
                    if let Some(pos) = st.create_group_members.iter().position(|id| id == &bid) {
                        st.create_group_members.remove(pos);
                        if st.create_group_lead.as_ref() == Some(&bid) {
                            st.create_group_lead = st.create_group_members.first().cloned();
                        }
                    } else if st.create_group_members.len() < swarmbot_contracts::MAX_GROUP_MEMBERS
                    {
                        if st.create_group_lead.is_none() {
                            st.create_group_lead = Some(bid.clone());
                        }
                        st.create_group_members.push(bid);
                    }
                }),
                on_pick_lead: Box::new(move |bid, st| {
                    st.create_group_lead = Some(bid);
                }),
                on_close: Box::new(on_close),
                on_submit: Box::new(on_submit),
            },
        )
    }

    fn render_edit_group(&self, s: &AppState) -> gpui::Stateful<gpui::Div> {
        let Some(conv_id) = s.edit_group_id.clone() else {
            return div().id("edit-group-empty");
        };
        let existing = s.conversation(&conv_id).cloned();
        let selected_bots = s.edit_group_members.clone();
        let active_lead = s
            .edit_group_lead
            .clone()
            .filter(|id| selected_bots.contains(id))
            .or_else(|| selected_bots.first().cloned());
        let state_h = self.state.clone();
        let commands = self.commands.clone();
        let title_input = self.edit_group_title_input.clone();
        let composer_save = self.composer.clone();
        let composer_close = self.composer.clone();

        let on_close = {
            let state_h = state_h.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                state_h.update(cx, |st, cx| {
                    st.edit_group_id = None;
                    st.edit_group_title_seed = None;
                    cx.notify();
                });
                composer_close.read(cx).focus_input(window, cx);
            }
        };

        let on_submit = {
            let state_h = state_h.clone();
            let selected_bots = selected_bots.clone();
            let active_lead = active_lead.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let typed = title_input.read(cx).content().trim().to_string();
                let (title_patch, members_patch, lead_patch) = match &existing {
                    Some(conv) => {
                        let old_title = state_h.read(cx).conversation_title(conv);
                        let title_patch =
                            (!typed.is_empty() && typed != old_title).then_some(typed);
                        let (old_members, old_lead) = match &conv.kind {
                            ConversationKind::Group { members, lead } => {
                                (Some(members), Some(lead))
                            }
                            _ => (None, None),
                        };
                        let members_patch =
                            (old_members != Some(&selected_bots)).then_some(selected_bots.clone());
                        let lead_patch = if active_lead.as_ref() != old_lead {
                            active_lead.clone()
                        } else {
                            None
                        };
                        (title_patch, members_patch, lead_patch)
                    }
                    None => (
                        (!typed.is_empty()).then_some(typed),
                        Some(selected_bots.clone()),
                        active_lead.clone(),
                    ),
                };
                if title_patch.is_some() || members_patch.is_some() || lead_patch.is_some() {
                    let _ = commands.send(Command::UpdateGroup {
                        id: conv_id.clone(),
                        title: title_patch,
                        members: members_patch,
                        lead: lead_patch,
                    });
                } else {
                    state_h.update(cx, |st, cx| {
                        st.edit_group_id = None;
                        st.edit_group_title_seed = None;
                        cx.notify();
                    });
                }
                composer_save.read(cx).focus_input(window, cx);
            }
        };

        self.render_group_modal(
            s,
            GroupModalConfig {
                id_prefix: "edit-group",
                heading: "Edit Group",
                submit_label: "Save Changes",
                title_input: &self.edit_group_title_input,
                selected_bots: &selected_bots,
                active_lead: active_lead.as_ref(),
                on_toggle_member: Box::new(move |bid, st| {
                    if let Some(pos) = st.edit_group_members.iter().position(|id| id == &bid) {
                        st.edit_group_members.remove(pos);
                        if st.edit_group_lead.as_ref() == Some(&bid) {
                            st.edit_group_lead = st.edit_group_members.first().cloned();
                        }
                    } else if st.edit_group_members.len() < swarmbot_contracts::MAX_GROUP_MEMBERS {
                        if st.edit_group_lead.is_none() {
                            st.edit_group_lead = Some(bid.clone());
                        }
                        st.edit_group_members.push(bid);
                    }
                }),
                on_pick_lead: Box::new(move |bid, st| {
                    st.edit_group_lead = Some(bid);
                }),
                on_close: Box::new(on_close),
                on_submit: Box::new(on_submit),
            },
        )
    }

    fn render_group_modal<'a>(
        &self,
        s: &AppState,
        cfg: GroupModalConfig<'a>,
    ) -> gpui::Stateful<gpui::Div> {
        let count = cfg.selected_bots.len();
        let can_submit = (swarmbot_contracts::MIN_GROUP_MEMBERS
            ..=swarmbot_contracts::MAX_GROUP_MEMBERS)
            .contains(&count);
        let state_h = self.state.clone();
        let on_toggle_member = std::rc::Rc::new(cfg.on_toggle_member);
        let on_pick_lead = std::rc::Rc::new(cfg.on_pick_lead);
        let on_close = std::rc::Rc::new(cfg.on_close);
        let prefix = cfg.id_prefix;

        let mut bot_list = div()
            .id(SharedString::from(format!("{prefix}-bot-list")))
            .flex_1()
            .max_h(px(200.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1();

        for bot in &s.bots {
            let bot_id = bot.id.clone();
            let checked = cfg.selected_bots.contains(&bot_id);
            let is_lead = checked && cfg.active_lead == Some(&bot_id);
            let state_row = state_h.clone();
            let toggle = on_toggle_member.clone();
            let bot_id_c = bot_id.clone();
            bot_list = bot_list.child(
                div()
                    .id(SharedString::from(format!("{prefix}-bot-{}", bot.id)))
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
                            )
                            .when(is_lead, |d| {
                                d.child(bits::badge("lead", theme::ACCENT, 0x0a1428))
                            }),
                    )
                    .child(div().text_sm().child(if checked { "✓" } else { "○" }))
                    .on_click(move |_, _, cx: &mut App| {
                        let bid = bot_id_c.clone();
                        state_row.update(cx, |st, cx| {
                            toggle(bid, st);
                            cx.notify();
                        });
                    }),
            );
        }

        if s.bots.len() < swarmbot_contracts::MIN_GROUP_MEMBERS {
            let msg = if s.bots.is_empty() {
                "No bots added yet. Add at least 2 bots first via '+' -> Add Bot."
            } else {
                "Add 1 more bot first via '+' -> Add Bot (minimum 2)."
            };
            bot_list = bot_list.child(
                div()
                    .p_3()
                    .rounded_xl()
                    .bg(rgb(theme::SURFACE_RAISED))
                    .text_xs()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(msg),
            );
        }

        let mut lead_row = div().w_full().flex().flex_wrap().items_center().gap_1p5();
        for member_id in cfg.selected_bots {
            let Some(bot) = s.bot(member_id) else {
                continue;
            };
            let selected_lead = cfg.active_lead == Some(member_id);
            let state_lead = state_h.clone();
            let pick = on_pick_lead.clone();
            let bid = member_id.clone();
            lead_row = lead_row.child(
                div()
                    .id(SharedString::from(format!("{prefix}-lead-{}", bot.id)))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_full()
                    .border_1()
                    .text_xs()
                    .when(selected_lead, |d| {
                        d.bg(rgb(0xede9fe))
                            .border_color(rgb(0x8b5cf6))
                            .text_color(rgb(0x5b21b6))
                            .font_weight(gpui::FontWeight::BOLD)
                    })
                    .when(!selected_lead, |d| {
                        d.bg(rgb(theme::SURFACE))
                            .border_color(rgb(theme::BORDER))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .hover(|h| h.bg(rgb(theme::SURFACE_RAISED)))
                    })
                    .child(
                        div()
                            .size(px(6.0))
                            .flex_none()
                            .rounded_full()
                            .bg(theme::bot_color(bot)),
                    )
                    .child(format!("@{}", bot.name))
                    .on_click(move |_, _, cx: &mut App| {
                        let bid = bid.clone();
                        state_lead.update(cx, |st, cx| {
                            pick(bid, st);
                            cx.notify();
                        });
                    }),
            );
        }

        let on_close_header = on_close.clone();
        let on_close_cancel = on_close.clone();
        let on_close_backdrop = on_close.clone();

        let mut card = div()
            .id(SharedString::from(format!("{prefix}-card")))
            .occlude()
            .w(px(460.0))
            .max_h(px(580.0))
            .rounded_2xl()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .shadow_lg()
            .p_5()
            .flex()
            .flex_col()
            .gap_3()
            .on_click(|_, _, cx| {
                cx.stop_propagation();
            })
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
                            .child(cfg.heading),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("{prefix}-close")))
                            .cursor_pointer()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .hover(|h| h.text_color(rgb(theme::TEXT)))
                            .child("✕")
                            .on_click(move |ev, window, cx| on_close_header(ev, window, cx)),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(format!(
                        "Select {}-{} bots to collaborate in this group.",
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
                    .child(cfg.title_input.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(rgb(theme::TEXT_DIM))
                    .child(format!(
                        "SELECT MEMBERS ({count}/{})",
                        swarmbot_contracts::MAX_GROUP_MEMBERS
                    )),
            )
            .child(bot_list);

        if !cfg.selected_bots.is_empty() {
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(theme::TEXT_DIM))
                            .child("LEAD BOT"),
                    )
                    .child(lead_row),
            );
        }

        card = card.child(
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
                        .id(SharedString::from(format!("{prefix}-cancel")))
                        .px_3()
                        .py_1p5()
                        .rounded_lg()
                        .cursor_pointer()
                        .text_sm()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .hover(|h| h.text_color(rgb(theme::TEXT)))
                        .child("Cancel")
                        .on_click(move |ev, window, cx| on_close_cancel(ev, window, cx)),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("{prefix}-submit")))
                        .px_4()
                        .py_1p5()
                        .rounded_lg()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .when(can_submit, |d| {
                            d.cursor_pointer()
                                .bg(rgb(theme::ACCENT))
                                .text_color(rgb(theme::BACKGROUND))
                                .on_click(cfg.on_submit)
                        })
                        .when(!can_submit, |d| {
                            d.bg(rgb(0xd1d5db)).text_color(rgb(theme::TEXT_DIM))
                        })
                        .child(cfg.submit_label),
                ),
        );

        modal_backdrop(
            SharedString::from(format!("{prefix}-backdrop")),
            card,
            move |ev, window, cx| on_close_backdrop(ev, window, cx),
        )
    }

    fn render_create_bot(&self, s: &AppState, cx: &App) -> gpui::Stateful<gpui::Div> {
        let state_close = self.state.clone();
        let composer_close = self.composer.clone();
        let on_close = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            state_close.update(cx, |st, cx| {
                st.create_bot_open = false;
                cx.notify();
            });
            composer_close.read(cx).focus_input(window, cx);
        };

        let state_create = self.state.clone();
        let commands = self.commands.clone();
        let composer_create = self.composer.clone();
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

        let live_handle = resolve_bot_handle(
            handle_input.read(cx).content(),
            label_input.read(cx).content(),
        );
        let can_create = !live_handle.is_empty();

        let chosen_backend = selected_backend.clone();
        let on_create = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let label_raw = label_input.read(cx).content().trim().to_string();
            let handle = resolve_bot_handle(handle_input.read(cx).content(), &label_raw);
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
            composer_create.read(cx).focus_input(window, cx);
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

        let card = div()
            .id("create-bot-card")
            .occlude()
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
            .on_click(|_, _, cx| {
                cx.stop_propagation();
            })
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
                            .child("Add Bot"),
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
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child("Configure a new autonomous bot with its own workspace."),
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
                            .on_click(on_close.clone()),
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
            );

        modal_backdrop("create-bot-backdrop", card, on_close)
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
        let now = Instant::now();
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
            Screen::Usage => usage::render(s, &self.state, &self.commands),
        };

        // The one renderer of `state.toast` and `state.conn`, above the main
        // column and above the conversation. It is `None` — zero elements, no
        // stolen height — while the app is connected and has nothing to say.
        let top_bar = top_bar_model(s, now).map(|model| {
            render_top_bar(
                &model,
                s.graph_open,
                cx.listener(Self::on_palette_click),
                cx.listener(Self::on_graph_click),
            )
        });

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
            .child(
                // The main column: the status row, then the screen. The inner
                // wrapper gives the screen a definite height to fill, so a
                // screen that lays itself out with `size_full` does not overflow
                // under the bar.
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .children(top_bar)
                    .child(div().flex_1().min_h_0().flex().flex_col().child(main)),
            )
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
        if s.plus_menu_open {
            root = root.child(self.render_plus_menu(s));
        }
        if s.palette_open {
            root = root.child(self.render_palette_overlay());
        }
        if s.create_group_open {
            root = root.child(self.render_create_group(s));
        }
        if s.edit_group_id.is_some() {
            root = root.child(self.render_edit_group(s));
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

type GroupBotCallback = Box<dyn Fn(BotId, &mut AppState)>;
type ModalClickCallback = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

struct GroupModalConfig<'a> {
    id_prefix: &'static str,
    heading: &'static str,
    submit_label: &'static str,
    title_input: &'a Entity<TextInput>,
    selected_bots: &'a [BotId],
    active_lead: Option<&'a BotId>,
    on_toggle_member: GroupBotCallback,
    on_pick_lead: GroupBotCallback,
    on_close: ModalClickCallback,
    on_submit: ModalClickCallback,
}

/// Wrap a modal card in an occluding dimmed backdrop that closes on outside click.
fn modal_backdrop(
    backdrop_id: impl Into<gpui::ElementId>,
    card: impl IntoElement,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(backdrop_id)
        .occlude()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.5,
        })
        .on_click(on_close)
        .child(card)
}

fn plus_menu_item(
    id: &str,
    icon: &str,
    icon_bg: u32,
    icon_fg: u32,
    title: &str,
    subtitle: &str,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id.to_string()))
        .flex()
        .items_center()
        .gap_2p5()
        .px_2p5()
        .py_2()
        .rounded_lg()
        .cursor_pointer()
        .hover(|h| h.bg(rgb(theme::SURFACE_RAISED)))
        .on_click(move |_, window, cx: &mut App| {
            cx.stop_propagation();
            on_click(window, cx);
        })
        .child(
            div()
                .size(px(28.0))
                .flex_none()
                .rounded_lg()
                .bg(rgb(icon_bg))
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(icon_fg))
                .child(icon.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT))
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .truncate()
                        .child(subtitle.to_string()),
                ),
        )
}

/// Resolve the canonical bot `@handle` from an explicit handle field or fall
/// back to the normalized display name when the handle field is left blank.
fn resolve_bot_handle(handle_raw: &str, label_raw: &str) -> String {
    let typed = normalize_bot_handle(handle_raw);
    if typed.is_empty() {
        normalize_bot_handle(label_raw)
    } else {
        typed
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

/// How long the bar says "resynced" after a gap, mirroring the window
/// [`crate::worker::spawn_ticker`] keeps redrawing for.
const RESYNC_LABEL_TTL: Duration = Duration::from_secs(4);

/// The connection line of the top bar: the honest label and its colour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopBarStatus {
    pub label: String,
    pub color: u32,
}

/// What the top bar would draw, derived from state and the current time alone.
///
/// This exists so the visibility decision is a **pure function**: a GPUI
/// element cannot be asserted on without a GPU, but "does the bar have anything
/// to say, and what" can be, and that is the part a reviewer cares about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopBarModel {
    pub status: TopBarStatus,
    /// The one-line status message, already checked against its expiry window.
    pub toast: Option<String>,
    pub brain_url: String,
}

/// What the top bar should show at `now`, or `None` when it has nothing to say.
///
/// `None` is load-bearing: a permanently rendered header would steal a row from
/// the transcript for a window that is connected and quiet, so the bar appears
/// only when there is news — a toast inside its window, a socket that is not
/// `Connected`, or the few seconds of "resynced" after a gap. This is the only
/// renderer of [`AppState::toast`] and [`ConnectionState`], so "invisible" here
/// means every failed send, failed save, failed login and offline state is
/// invisible to the owner.
pub fn top_bar_model(s: &AppState, now: Instant) -> Option<TopBarModel> {
    let resynced = s
        .resynced_at
        .is_some_and(|at| now.saturating_duration_since(at) < RESYNC_LABEL_TTL);
    let toast = s.visible_toast(now).map(str::to_string);
    if toast.is_none() && s.conn.is_online() && !resynced {
        return None;
    }
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
    Some(TopBarModel {
        status: TopBarStatus { label, color },
        toast,
        brain_url: s.brain_url.clone(),
    })
}

/// The status row above the main pane: the connection, the brain's URL and the
/// last action's message. Only built when [`top_bar_model`] has something to
/// say, so it costs no vertical space while the app is connected and quiet.
fn render_top_bar(
    model: &TopBarModel,
    graph_open: bool,
    on_palette: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_graph: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
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
        .child(bits::dot(model.status.color, px(9.0)))
        .child(
            div()
                .text_xs()
                .text_color(rgb(model.status.color))
                .child(model.status.label.clone()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .child(model.brain_url.clone()),
        )
        .children(model.toast.as_ref().map(|toast| {
            // A long failure message must not push the buttons off the row.
            div()
                .min_w_0()
                .max_w(px(520.0))
                .truncate()
                .text_xs()
                .text_color(rgb(theme::ACCENT_HOVER))
                .child(toast.clone())
        }))
        .child(div().flex_1())
        .child(
            bits::button(if graph_open { "Hide graph" } else { "Graph" }, Tone::Ghost)
                .id("toggle-graph")
                .on_click(on_graph),
        )
        .child(
            bits::button("Palette", Tone::Ghost)
                .id("toggle-palette")
                .on_click(on_palette),
        )
}

#[cfg(test)]
mod tests {
    use super::{resolve_bot_handle, top_bar_model};
    use crate::state::{AppState, ConnectionState, TOAST_TTL};
    use crate::theme;
    use std::time::{Duration, Instant};

    #[test]
    fn resolve_bot_handle_prefers_explicit_handle_or_falls_back_to_display_name() {
        assert_eq!(
            resolve_bot_handle("@CodeReviewer", "Display"),
            "codereviewer"
        );
        assert_eq!(resolve_bot_handle("  ", "Code Reviewer"), "code-reviewer");
        assert_eq!(resolve_bot_handle("   ", "   "), "");
    }

    fn connected_state() -> AppState {
        let mut state = AppState::new("http://brain.test");
        state.conn = ConnectionState::Connected;
        state
    }

    /// The bar is the only renderer of the toast and the connection state, so
    /// "it renders nothing here" is exactly "the owner is told nothing".
    #[test]
    fn the_top_bar_is_empty_when_connected_with_no_toast() {
        let state = connected_state();
        assert!(
            top_bar_model(&state, Instant::now()).is_none(),
            "a connected, quiet window gets no status row and loses no height"
        );
    }

    #[test]
    fn the_top_bar_shows_a_toast_and_hides_it_once_the_window_is_over() {
        let mut state = connected_state();
        let set_at = Instant::now();
        state.set_toast("send failed: connection refused", set_at);

        let model = top_bar_model(&state, set_at).expect("a toast must be visible");
        assert_eq!(
            model.toast.as_deref(),
            Some("send failed: connection refused")
        );
        assert_eq!(model.status.label, "connected");
        assert_eq!(model.status.color, theme::SUCCESS);
        assert_eq!(model.brain_url, "http://brain.test");

        // Inside the window the bar stays.
        let inside = set_at + TOAST_TTL - Duration::from_millis(1);
        assert!(top_bar_model(&state, inside).is_some());

        // Past it the bar is gone even before the ticker has cleared the field:
        // a view must never show a message the state has outlived.
        assert!(state.toast.is_some(), "the ticker has not run yet");
        assert!(
            top_bar_model(&state, set_at + TOAST_TTL).is_none(),
            "an expired toast must not keep the row on screen"
        );
    }

    #[test]
    fn the_top_bar_reports_every_disconnected_state_in_its_honest_colour() {
        let now = Instant::now();
        let cases = [
            (ConnectionState::Connecting, "connecting…", theme::WARNING),
            (
                ConnectionState::Reconnecting { attempt: 2 },
                "reconnecting (attempt 2)",
                theme::WARNING,
            ),
            (
                ConnectionState::Offline {
                    reason: "socket closed".to_string(),
                },
                "offline: socket closed",
                theme::DANGER,
            ),
        ];
        for (conn, label, color) in cases {
            let mut state = AppState::new("http://brain.test");
            state.conn = conn.clone();
            let model = top_bar_model(&state, now)
                .unwrap_or_else(|| panic!("{conn:?} must be visible to the owner"));
            assert_eq!(model.status.label, label, "{conn:?}");
            assert_eq!(model.status.color, color, "{conn:?}");
            assert_eq!(model.toast, None, "{conn:?} has no toast to show");
        }
    }

    #[test]
    fn the_resynced_notice_is_visible_for_a_few_seconds_and_then_goes_quiet() {
        let mut state = connected_state();
        let resynced_at = Instant::now();
        state.resynced_at = Some(resynced_at);

        let model = top_bar_model(&state, resynced_at).expect("the resync notice is visible");
        assert_eq!(model.status.label, "resynced");
        assert_eq!(model.status.color, theme::SUCCESS);

        assert!(
            top_bar_model(&state, resynced_at + Duration::from_secs(5)).is_none(),
            "after the notice's window the bar is quiet again"
        );
    }
}

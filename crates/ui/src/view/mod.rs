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
    Window, div, list, prelude::*, px, rgb,
};
use swarmbot_contracts::{EntryId, EntryKind, FormField, RequestId};

use crate::RENDERS;
use crate::actions::{CancelOverlay, FocusComposer, StopTurn, ToggleGraph, TogglePalette};
use crate::proto::{Command, SecretToken};
use crate::state::{AppState, ConnectionState, Screen};
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

        // An empty conversation shows the simulated history matching the design screenshot
        let transcript_area = if count == 0 {
            conversation::mock_conversation_view().into_any_element()
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
        root
    }
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

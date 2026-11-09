//! The command palette (`ctrl-k`): fuzzy-find a bot, conversation or task and
//! jump to it.
//!
//! The palette is its own focused overlay with a `"Palette"` key context, so its
//! arrows/enter navigate the results while the composer keeps its own bindings.
//! The result list uses [`gpui::uniform_list`] — the rows are single-line and
//! therefore uniform-height, which is exactly the case `uniform_list` is for.
//! Matching is a pure subsequence test ([`fuzzy`]) a `#[gpui::test]` asserts on.

use std::ops::Range;
use std::sync::mpsc::Sender;

use gpui::{
    App, Context, Entity, IntoElement, SharedString, Subscription, Window, div, prelude::*, px,
    rgb, uniform_list,
};
use swarmbot_contracts::{BotId, ConversationId, TaskId};

use crate::actions::{SelectConfirm, SelectNext, SelectPrev};
use crate::proto::Command;
use crate::state::{AppState, Screen};
use crate::theme;
use crate::widgets::bits;
use crate::widgets::text_input::TextInput;

/// Case-insensitive subsequence match: every char of `query` appears in `text`
/// in order. An empty query matches everything.
pub fn fuzzy(query: &str, text: &str) -> bool {
    let mut text = text.chars().flat_map(char::to_lowercase);
    'outer: for qc in query.chars().flat_map(char::to_lowercase) {
        for tc in text.by_ref() {
            if tc == qc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

#[derive(Clone, Debug, PartialEq)]
pub enum PaletteAction {
    OpenBot(BotId),
    OpenConversation(ConversationId),
    OpenTask(TaskId, ConversationId),
    OpenScreen(Screen),
}

#[derive(Clone, Debug)]
pub struct PaletteItem {
    pub title: String,
    pub subtitle: String,
    pub action: PaletteAction,
}

pub struct Palette {
    input: Entity<TextInput>,
    state: Entity<AppState>,
    commands: Sender<Command>,
    results: Vec<PaletteItem>,
    selected: usize,
    _observe: Subscription,
}

impl Palette {
    pub fn new(state: Entity<AppState>, commands: Sender<Command>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Jump to a bot, conversation or task…", cx));
        let results = Self::compute(state.read(cx), "");
        let observe = cx.observe(&input, |this, input, cx| {
            let query = input.read(cx).content().to_string();
            let s = this.state.read(cx);
            this.results = Self::compute(s, &query);
            this.selected = 0;
            cx.notify();
        });
        Self {
            input,
            state,
            commands,
            results,
            selected: 0,
            _observe: observe,
        }
    }

    pub fn focus_input(&self, window: &mut Window, cx: &App) {
        self.input.read(cx).focus(window);
    }

    fn compute(s: &AppState, query: &str) -> Vec<PaletteItem> {
        let mut items = Vec::new();
        for bot in &s.bots {
            items.push(PaletteItem {
                title: bot.label.clone(),
                subtitle: format!("@{} · bot", bot.name),
                action: PaletteAction::OpenBot(bot.id.clone()),
            });
        }
        for conversation in &s.conversations {
            let title = s.conversation_title(conversation);
            let kind = if conversation.is_direct() {
                "DM"
            } else {
                "group"
            };
            items.push(PaletteItem {
                title,
                subtitle: format!("{kind} · conversation"),
                action: PaletteAction::OpenConversation(conversation.id.clone()),
            });
        }
        for task in &s.tasks {
            let owner = s
                .bot(&task.owner)
                .map(|b| format!("@{}", b.name))
                .unwrap_or_else(|| task.owner.as_str().to_string());
            items.push(PaletteItem {
                title: task.title.clone(),
                subtitle: format!("{owner} · {} · task", task.status.label()),
                action: PaletteAction::OpenTask(task.id.clone(), task.origin.clone()),
            });
        }
        for routine in &s.routines {
            let target = s
                .bot(&routine.bot_id)
                .map(|b| format!("@{}", b.name))
                .unwrap_or_else(|| routine.bot_id.as_str().to_string());
            let status = match routine.status {
                swarmbot_contracts::RoutineStatus::Active => "active",
                swarmbot_contracts::RoutineStatus::Paused => "paused",
                swarmbot_contracts::RoutineStatus::Completed => "completed",
            };
            items.push(PaletteItem {
                title: routine.name.clone(),
                subtitle: format!("{target} · {status} · routine"),
                action: PaletteAction::OpenScreen(Screen::Routines),
            });
        }
        items.push(PaletteItem {
            title: "Routines".to_string(),
            subtitle: "screen · scheduled triggers".to_string(),
            action: PaletteAction::OpenScreen(Screen::Routines),
        });
        items.push(PaletteItem {
            title: "Nodes".to_string(),
            subtitle: "screen · paired execution nodes".to_string(),
            action: PaletteAction::OpenScreen(Screen::Nodes),
        });
        items.push(PaletteItem {
            title: "Settings".to_string(),
            subtitle: "screen · brain configuration & backends".to_string(),
            action: PaletteAction::OpenScreen(Screen::Settings),
        });
        let q = query.trim();
        let mut matched: Vec<PaletteItem> = items
            .into_iter()
            .filter(|item| q.is_empty() || fuzzy(q, &item.title) || fuzzy(q, &item.subtitle))
            .collect();
        matched.truncate(50);
        matched
    }

    fn jump(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.results.get(index).cloned() else {
            return;
        };
        let action = item.action.clone();
        // Jumping to a task also fetches its steps so the graph detail is ready.
        if let PaletteAction::OpenTask(task_id, _) = &action {
            let _ = self.commands.send(Command::TaskDetail(task_id.clone()));
        }
        self.state.update(cx, |s, cx| {
            match action.clone() {
                PaletteAction::OpenBot(bot_id) => {
                    if let Some(bot) = s.bot(&bot_id) {
                        let dm = bot.direct_conversation_id.clone();
                        s.open(dm);
                    }
                }
                PaletteAction::OpenConversation(id) => s.open(id),
                PaletteAction::OpenTask(task_id, origin) => {
                    s.open_task = Some(task_id);
                    s.open(origin);
                    s.graph_open = true;
                    s.screen = Screen::Conversation;
                }
                PaletteAction::OpenScreen(screen) => {
                    s.screen = screen;
                }
            }
            s.palette_open = false;
            cx.notify();
        });
        if matches!(action, PaletteAction::OpenScreen(Screen::Routines)) {
            let _ = self.commands.send(Command::FetchRoutines);
        } else if matches!(action, PaletteAction::OpenScreen(Screen::Nodes)) {
            let _ = self.commands.send(Command::FetchNodes);
        } else if matches!(action, PaletteAction::OpenScreen(Screen::Settings)) {
            let _ = self.commands.send(Command::FetchSettings);
            let _ = self.commands.send(Command::FetchBackends);
        }
    }

    fn on_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1) % self.results.len();
            cx.notify();
        }
    }

    fn on_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        if !self.results.is_empty() {
            self.selected = (self.selected + self.results.len() - 1) % self.results.len();
            cx.notify();
        }
    }

    fn on_confirm(&mut self, _: &SelectConfirm, _: &mut Window, cx: &mut Context<Self>) {
        self.jump(self.selected, cx);
    }

    fn row(&self, index: usize) -> gpui::AnyElement {
        let item = &self.results[index];
        let selected = index == self.selected;
        let state = self.state.clone();
        let commands = self.commands.clone();
        let action = item.action.clone();
        div()
            .id(SharedString::from(format!("palette-{index}")))
            .h(px(36.0))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .cursor_pointer()
            .when(selected, |d| d.bg(rgb(theme::SURFACE_RAISED)))
            .child(bits::dot(
                if selected {
                    theme::ACCENT
                } else {
                    theme::BORDER
                },
                px(7.0),
            ))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::TEXT))
                    .child(item.title.clone()),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child(item.subtitle.clone()),
            )
            .on_click(move |_, _, cx: &mut App| {
                if let PaletteAction::OpenTask(task_id, _) = &action {
                    let _ = commands.send(Command::TaskDetail(task_id.clone()));
                }
                state.update(cx, |s, cx| {
                    match action.clone() {
                        PaletteAction::OpenBot(bot_id) => {
                            if let Some(bot) = s.bot(&bot_id) {
                                let dm = bot.direct_conversation_id.clone();
                                s.open(dm);
                            }
                        }
                        PaletteAction::OpenConversation(id) => s.open(id),
                        PaletteAction::OpenTask(task_id, origin) => {
                            s.open_task = Some(task_id);
                            s.open(origin);
                            s.graph_open = true;
                        }
                        PaletteAction::OpenScreen(screen) => {
                            s.screen = screen;
                        }
                    }
                    s.palette_open = false;
                    cx.notify();
                });
                if matches!(action, PaletteAction::OpenScreen(Screen::Routines)) {
                    let _ = commands.send(Command::FetchRoutines);
                } else if matches!(action, PaletteAction::OpenScreen(Screen::Nodes)) {
                    let _ = commands.send(Command::FetchNodes);
                } else if matches!(action, PaletteAction::OpenScreen(Screen::Settings)) {
                    let _ = commands.send(Command::FetchSettings);
                    let _ = commands.send(Command::FetchBackends);
                }
            })
            .into_any_element()
    }
}

impl gpui::Render for Palette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.results.len();
        let height = px((count.clamp(1, 8) as f32) * 36.0);
        let list = uniform_list(
            "palette-results",
            count,
            cx.processor(|this, range: Range<usize>, _window, _cx| {
                range.map(|i| this.row(i)).collect::<Vec<_>>()
            }),
        )
        .h(height);

        div()
            .key_context("Palette")
            .on_action(cx.listener(Self::on_next))
            .on_action(cx.listener(Self::on_prev))
            .on_action(cx.listener(Self::on_confirm))
            .w(px(560.0))
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::ACCENT))
            .bg(rgb(theme::SURFACE))
            .shadow_lg()
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.input.clone())
            .child(list)
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("↑↓ to move · Enter to jump · Esc to close"),
            )
    }
}

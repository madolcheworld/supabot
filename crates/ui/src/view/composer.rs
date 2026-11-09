//! The composer: a multi-line input where Enter sends, Shift+Enter is a newline,
//! and `@` opens a member picker that splices in a mention.
//!
//! The picker logic is split into pure functions ([`mention_query`],
//! [`match_members`], [`addressed_bots`]) so a `#[test]` can assert on it
//! without a window. The [`Composer`] entity owns the text field and the picker
//! state, and recomputes the picker whenever the field notifies (an observer), so
//! the matches always track what is actually typed.

use std::ops::Range;
use std::sync::mpsc::Sender;

use gpui::{
    App, Context, Entity, IntoElement, SharedString, Subscription, Window, div, prelude::*, px, rgb,
};
use swarmbot_contracts::{Bot, BotId};

use crate::actions::{SelectNext, SelectPrev, Send as SendAction};
use crate::proto::Command;
use crate::state::AppState;
use crate::theme;
use crate::widgets::bits::{self, Tone};
use crate::widgets::text_input::TextInput;

/// The `@token` currently being typed, if the caret sits in one. Returns the byte
/// range to replace (the whole `@query`) and the query text after the `@`.
///
/// A token counts only when the `@` starts the text or follows whitespace, and no
/// whitespace sits between it and the caret — so `a @b` is a mention but `a@b`
/// (an email) is not.
pub fn mention_query(text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
    let cursor = cursor.min(text.len());
    let upto = &text[..cursor];
    let mut at = None;
    for (idx, ch) in upto.char_indices().rev() {
        if ch == '@' {
            at = Some(idx);
            break;
        }
        if ch.is_whitespace() {
            break;
        }
    }
    let at = at?;
    let preceded_by_boundary =
        at == 0 || upto[..at].chars().last().is_some_and(char::is_whitespace);
    if !preceded_by_boundary {
        return None;
    }
    Some((at..cursor, upto[at + 1..].to_string()))
}

/// Rank the members matching `query`: name-prefix, then label-prefix, then any
/// substring; ties keep roster order. An empty query lists every member.
pub fn match_members(query: &str, members: &[Bot]) -> Vec<BotId> {
    let q = query.trim().to_lowercase();
    let mut scored: Vec<(u8, BotId)> = members
        .iter()
        .filter_map(|bot| {
            let name = bot.name.to_lowercase();
            let label = bot.label.to_lowercase();
            let score = if q.is_empty() || name.starts_with(&q) {
                0
            } else if label.starts_with(&q) {
                1
            } else if name.contains(&q) || label.contains(&q) {
                2
            } else {
                return None;
            };
            Some((score, bot.id.clone()))
        })
        .collect();
    scored.sort_by_key(|(score, _)| *score);
    scored.into_iter().map(|(_, id)| id).collect()
}

/// Every member mentioned with `@handle` in `text`, de-duplicated, in order.
pub fn addressed_bots(text: &str, members: &[Bot]) -> Vec<BotId> {
    let mut out: Vec<BotId> = Vec::new();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (idx, ch) = chars[i];
        let at_boundary = idx == 0 || text[..idx].chars().last().is_some_and(char::is_whitespace);
        if ch == '@' && at_boundary {
            let mut j = i + 1;
            let mut token = String::new();
            while j < chars.len() {
                let (_, c) = chars[j];
                if c.is_alphanumeric() || c == '_' || c == '-' {
                    token.push(c);
                    j += 1;
                } else {
                    break;
                }
            }
            if !token.is_empty() {
                let lower = token.to_lowercase();
                if let Some(bot) = members.iter().find(|b| b.name.to_lowercase() == lower)
                    && !out.contains(&bot.id)
                {
                    out.push(bot.id.clone());
                }
            }
            i = j.max(i + 1);
            continue;
        }
        i += 1;
    }
    out
}

struct Picker {
    range: Range<usize>,
    matches: Vec<BotId>,
    selected: usize,
}

pub struct Composer {
    input: Entity<TextInput>,
    state: Entity<AppState>,
    commands: Sender<Command>,
    picker: Option<Picker>,
    _observe: Subscription,
    _observe_state: Subscription,
}

impl Composer {
    pub fn new(state: Entity<AppState>, commands: Sender<Command>, cx: &mut Context<Self>) -> Self {
        let disabled = !state.read(cx).conn.is_online();
        let input = cx.new(|cx| {
            TextInput::new("Message…  ( @ to mention )", cx)
                .multiline()
                .disabled(disabled)
        });
        let observe = cx.observe(&input, |this, input, cx| this.recompute(input, cx));
        // The birth flag above is stale the moment the worker connects: the
        // window is built before the live socket exists, so without this sync
        // the field stays disabled *behind a footer that already says "Enter
        // sends"* — it looks live and swallows every keystroke. `set_disabled`
        // is a no-op unless the value actually flips, so riding every state
        // notify is cheap.
        let observe_state = cx.observe(&state, |this, state, cx| {
            let online = state.read(cx).conn.is_online();
            this.input
                .update(cx, |input, cx| input.set_disabled(!online, cx));
        });
        Self {
            input,
            state,
            commands,
            picker: None,
            _observe: observe,
            _observe_state: observe_state,
        }
    }

    pub fn focus_input(&self, window: &mut Window, cx: &App) {
        self.input.read(cx).focus(window);
    }

    fn member_bots(&self, cx: &App) -> Vec<Bot> {
        let s = self.state.read(cx);
        let Some(conversation) = s.open_conversation() else {
            return Vec::new();
        };
        s.members(&conversation.id)
            .iter()
            .filter_map(|id| s.bot(id).cloned())
            .collect()
    }

    fn recompute(&mut self, input: Entity<TextInput>, cx: &mut Context<Self>) {
        let (content, cursor) = {
            let ti = input.read(cx);
            (ti.content().to_string(), ti.cursor())
        };
        let members = self.member_bots(cx);
        match mention_query(&content, cursor) {
            Some((range, query)) => {
                let matches = match_members(&query, &members);
                if matches.is_empty() {
                    self.picker = None;
                } else {
                    let selected = self
                        .picker
                        .as_ref()
                        .filter(|p| p.range.start == range.start)
                        .map(|p| p.selected.min(matches.len() - 1))
                        .unwrap_or(0);
                    self.picker = Some(Picker {
                        range,
                        matches,
                        selected,
                    });
                }
            }
            None => self.picker = None,
        }
        cx.notify();
    }

    fn confirm_picker(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let Some(bot_id) = picker.matches.get(picker.selected) else {
            return;
        };
        let name = self
            .state
            .read(cx)
            .bot(bot_id)
            .map(|b| b.name.clone())
            .unwrap_or_else(|| bot_id.as_str().to_string());
        let range = picker.range.clone();
        let replacement = format!("@{name} ");
        self.input
            .update(cx, |ti, cx| ti.splice(range, &replacement, cx));
        cx.notify();
    }

    fn on_send(&mut self, _: &SendAction, _: &mut Window, cx: &mut Context<Self>) {
        // Enter confirms an open picker; otherwise it sends.
        if self.picker.is_some() {
            self.confirm_picker(cx);
            return;
        }
        self.send_now(cx);
    }

    fn send_now(&mut self, cx: &mut Context<Self>) {
        let online = self.state.read(cx).conn.is_online();
        let conversation = self.state.read(cx).open_conversation.clone();
        let text = self.input.read(cx).content().to_string();
        if text.trim().is_empty() {
            return;
        }
        let Some(conversation) = conversation else {
            self.set_toast("no conversation is open", cx);
            return;
        };
        if !online {
            self.set_toast("offline — the message was not sent", cx);
            return;
        }
        let members = self.member_bots(cx);
        let addressed = addressed_bots(&text, &members);
        let _ = self.commands.send(Command::Send {
            conversation,
            text,
            addressed,
            reply_to: None,
        });
        self.input.update(cx, |ti, cx| ti.clear(cx));
        self.picker = None;
        cx.notify();
    }

    fn set_toast(&self, message: &str, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.toast = Some(message.to_string());
            cx.notify();
        });
    }

    fn on_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = &mut self.picker
            && !picker.matches.is_empty()
        {
            picker.selected = (picker.selected + 1) % picker.matches.len();
            cx.notify();
            return;
        }
        self.input.update(cx, |ti, cx| ti.move_vertically(1, cx));
    }

    fn on_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = &mut self.picker
            && !picker.matches.is_empty()
        {
            picker.selected = (picker.selected + picker.matches.len() - 1) % picker.matches.len();
            cx.notify();
            return;
        }
        self.input.update(cx, |ti, cx| ti.move_vertically(-1, cx));
    }

    /// Close the picker (Escape). Returns whether one was open, so the root can
    /// fall through to closing the palette.
    pub fn close_picker(&mut self, cx: &mut Context<Self>) -> bool {
        let was = self.picker.is_some();
        self.picker = None;
        cx.notify();
        was
    }
}

impl gpui::Render for Composer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (online, reason) = {
            let s = self.state.read(cx);
            (s.conn.is_online(), s.conn.label())
        };

        let mut body = div()
            .key_context("Composer")
            .on_action(cx.listener(Self::on_send))
            .on_action(cx.listener(Self::on_next))
            .on_action(cx.listener(Self::on_prev))
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .border_t_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .p_2();

        // Mention picker, above the field.
        if let Some(picker) = &self.picker {
            let mut popup = div()
                .id("mention-popup")
                .w_full()
                .max_h(px(180.0))
                .overflow_y_scroll()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::ACCENT))
                .bg(rgb(theme::SURFACE_RAISED))
                .p_1()
                .flex()
                .flex_col()
                .gap_0p5();
            let matches = picker.matches.clone();
            for (i, bot_id) in matches.iter().enumerate() {
                let selected = i == picker.selected;
                let bot = self.state.read(cx).bot(bot_id).cloned();
                let (label, color) = match &bot {
                    Some(b) => (b.label.clone(), theme::bot_color(b)),
                    None => (
                        bot_id.as_str().to_string(),
                        theme::bot_color_from_id(bot_id.as_str()),
                    ),
                };
                popup = popup.child(
                    div()
                        .id(SharedString::from(format!("mention-{i}")))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(rgb(theme::ACCENT)))
                        .child(bits::avatar(color, bits::initial(&label), px(18.0)))
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(if selected { 0xffffff } else { theme::TEXT }))
                                .child(format!("@{}", bot.map(|b| b.name).unwrap_or_default())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(if selected { 0xdfe7ff } else { theme::TEXT_DIM }))
                                .child(label),
                        ),
                );
            }
            body = body.child(popup);
        }

        body = body.child(self.input.clone());

        // Footer: hints, or the offline reason, plus Send.
        let footer = if online {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child("Enter sends · Shift+Enter newline · @ mentions"),
                )
                .child(
                    bits::button("Send", Tone::Primary)
                        .id("composer-send")
                        .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                            this.send_now(cx);
                        })),
                )
        } else {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .text_xs()
                        .text_color(rgb(theme::DANGER))
                        .child(format!("disabled — {reason}")),
                )
                .child(bits::disabled_button("Send"))
        };
        body.child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::{Bot, ConversationId};

    fn bot(name: &str, label: &str) -> Bot {
        Bot::new(name, label, "", "fake", ConversationId::new())
    }

    #[test]
    fn mention_query_finds_the_token_under_the_caret() {
        let text = "hello @sc";
        let (range, query) = mention_query(text, text.len()).expect("a mention");
        assert_eq!(query, "sc");
        assert_eq!(&text[range.clone()], "@sc");
        // An email-shaped token is not a mention.
        assert!(mention_query("a@b", 3).is_none());
        // A completed mention followed by a space is no longer "being typed".
        assert!(mention_query("@scout ", 7).is_none());
        // A bare '@' opens the picker with an empty query.
        let bare = "cc @";
        let (range, query) = mention_query(bare, 4).expect("a mention");
        assert_eq!(query, "");
        assert_eq!(&bare[range], "@");
    }

    #[test]
    fn match_members_ranks_name_prefix_first() {
        let members = vec![
            bot("scout", "Scout"),
            bot("dev", "Developer"),
            bot("sentinel", "Sentinel"),
        ];
        let by_s = match_members("s", &members);
        assert_eq!(by_s.len(), 2, "scout + sentinel match on name prefix");
        assert!(by_s.contains(&members[0].id) && by_s.contains(&members[2].id));
        let by_dev = match_members("dev", &members);
        assert_eq!(by_dev[0], members[1].id);
        assert_eq!(
            match_members("", &members).len(),
            3,
            "empty query lists all"
        );
    }

    #[test]
    fn addressed_bots_extracts_and_dedupes_mentions() {
        let members = vec![bot("scout", "Scout"), bot("dev", "Dev")];
        let got = addressed_bots("hey @scout and @dev, cc @scout again", &members);
        assert_eq!(got.len(), 2, "a repeated mention is counted once");
        assert!(got.contains(&members[0].id) && got.contains(&members[1].id));
        assert!(addressed_bots("@nobody here", &members).is_empty());
    }
}

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
use swarmbot_contracts::{Author, Bot, BotId};

use crate::actions::{SelectNext, SelectPrev, Send as SendAction};
use crate::proto::Command;
use crate::state::AppState;
use crate::theme;
use crate::widgets::bits;
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

/// One item selectable in the `@` mention picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MentionOption {
    Everyone,
    Bot(BotId),
}

/// Rank the mention candidates matching `query`. In group conversations,
/// `@everyone` is offered alongside member bots when `"everyone"` matches.
pub fn match_mentions(query: &str, members: &[Bot], is_group: bool) -> Vec<MentionOption> {
    let q = query.trim().to_lowercase();
    let mut out = Vec::new();
    let everyone_prefix = is_group && "everyone".starts_with(&q);
    if everyone_prefix {
        out.push(MentionOption::Everyone);
    }
    for id in match_members(query, members) {
        out.push(MentionOption::Bot(id));
    }
    if is_group && !everyone_prefix && "everyone".contains(&q) {
        out.push(MentionOption::Everyone);
    }
    out
}

/// Dynamic placeholder string for the composer given the currently open
/// conversation.
pub fn composer_placeholder(s: &AppState) -> String {
    match s.open_conversation() {
        Some(conv) if conv.kind.is_group() => format!("Message #{}", s.conversation_title(conv)),
        Some(conv) => {
            let handle = conv
                .direct_bot()
                .and_then(|id| s.bot(id))
                .map(|b| b.name.as_str());
            match handle {
                Some(name) => format!("Message @{name}"),
                None => format!("Message {}", s.conversation_title(conv)),
            }
        }
        None => "Select a conversation…".to_string(),
    }
}

/// Check if a word/name appears in `text` on word boundaries, case-insensitively.
fn contains_word(text: &str, word: &str) -> bool {
    if word.is_empty() || text.is_empty() {
        return false;
    }
    let word_lower = word.to_lowercase();
    let text_lower = text.to_lowercase();

    let mut search_from = 0;
    while let Some(byte_idx) = text_lower[search_from..].find(&word_lower) {
        let match_start = search_from + byte_idx;
        let match_end = match_start + word_lower.len();

        let before_ok = if match_start == 0 {
            true
        } else {
            let prev_slice = &text_lower[..match_start];
            match prev_slice.chars().last() {
                Some('@') => {
                    let before_at = &prev_slice[..prev_slice.len() - 1];
                    before_at
                        .chars()
                        .last()
                        .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '.')
                }
                Some(last_char) => {
                    !last_char.is_alphanumeric() && last_char != '_' && last_char != '-'
                }
                None => true,
            }
        };

        let after_ok = if match_end >= text_lower.len() {
            true
        } else {
            text_lower[match_end..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '-')
        };

        if before_ok && after_ok {
            return true;
        }

        if let Some(ch) = text_lower[match_start..].chars().next() {
            search_from = match_start + ch.len_utf8();
        } else {
            break;
        }
    }
    false
}

/// True when `text` explicitly addresses `@everyone` at a mention boundary.
fn has_at_everyone(text: &str) -> bool {
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
            if token.eq_ignore_ascii_case("everyone") {
                return true;
            }
            i = j.max(i + 1);
            continue;
        }
        i += 1;
    }
    false
}

/// Every member mentioned with `@handle` or naturally addressed by name in `text`,
/// de-duplicated, in order.
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

    // Also match natural mentions (bot names or labels on word boundaries)
    for bot in members {
        if (contains_word(text, &bot.name) || contains_word(text, &bot.label))
            && !out.contains(&bot.id)
        {
            out.push(bot.id.clone());
        }
    }

    out
}

struct Picker {
    range: Range<usize>,
    matches: Vec<MentionOption>,
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
        let (disabled, initial_placeholder) = {
            let s = state.read(cx);
            (!s.conn.is_online(), composer_placeholder(s))
        };
        let input = cx.new(|cx| {
            TextInput::new(&initial_placeholder, cx)
                .multiline()
                .disabled(disabled)
        });
        let observe = cx.observe(&input, |this, input, cx| this.recompute(input, cx));
        // The birth flag above is stale the moment the worker connects: the
        // window is built before the live socket exists, so without this sync
        // the field stays disabled *behind a footer that already says "Enter
        // sends"* — it looks live and swallows every keystroke. `set_disabled`
        // and `set_placeholder` are no-ops unless values change, so riding
        // every state notify is cheap.
        let observe_state = cx.observe(&state, |this, state, cx| {
            let s = state.read(cx);
            let online = s.conn.is_online();
            let placeholder = composer_placeholder(s);
            this.input.update(cx, |input, cx| {
                input.set_disabled(!online, cx);
                input.set_placeholder(placeholder, cx);
            });
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

    fn member_bots(&self, cx: &App) -> (bool, Vec<Bot>) {
        let s = self.state.read(cx);
        let Some(conversation) = s.open_conversation() else {
            return (false, Vec::new());
        };
        let is_group = conversation.kind.is_group();
        let bots = s
            .members(&conversation.id)
            .iter()
            .filter_map(|id| s.bot(id).cloned())
            .collect();
        (is_group, bots)
    }

    fn recompute(&mut self, input: Entity<TextInput>, cx: &mut Context<Self>) {
        let (content, cursor) = {
            let ti = input.read(cx);
            (ti.content().to_string(), ti.cursor())
        };
        let (is_group, members) = self.member_bots(cx);
        match mention_query(&content, cursor) {
            Some((range, query)) => {
                let matches = match_mentions(&query, &members, is_group);
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

    fn insert_mention(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let Some(option) = picker.matches.get(index) else {
            return;
        };
        let replacement = match option {
            MentionOption::Everyone => "@everyone ".to_string(),
            MentionOption::Bot(bot_id) => {
                let name = self
                    .state
                    .read(cx)
                    .bot(bot_id)
                    .map(|b| b.name.clone())
                    .unwrap_or_else(|| bot_id.as_str().to_string());
                format!("@{name} ")
            }
        };
        let range = picker.range.clone();
        self.input
            .update(cx, |ti, cx| ti.splice(range, &replacement, cx));
        cx.notify();
    }

    fn confirm_picker(&mut self, cx: &mut Context<Self>) {
        let idx = self.picker.as_ref().map(|p| p.selected).unwrap_or(0);
        self.insert_mention(idx, cx);
    }

    fn pick_mention(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_mention(index, cx);
        self.focus_input(window, cx);
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
        let (online, conversation, reply_to) = {
            let s = self.state.read(cx);
            (
                s.conn.is_online(),
                s.open_conversation.clone(),
                s.reply_to.clone(),
            )
        };
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
        let (is_group, members) = self.member_bots(cx);
        // GroupRouter only enters its `@everyone` broadcast path when explicit
        // `addressed` is empty; pass empty when `@everyone` is present so an
        // accompanying `@bot` doesn't suppress the broadcast.
        let addressed = if is_group && has_at_everyone(&text) {
            Vec::new()
        } else {
            addressed_bots(&text, &members)
        };
        let _ = self.commands.send(Command::Send {
            conversation,
            text,
            addressed,
            reply_to,
        });
        if self.state.read(cx).reply_to.is_some() {
            self.state.update(cx, |s, cx| {
                s.reply_to = None;
                cx.notify();
            });
        }
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
        let (online, running_conv, reply_preview) = {
            let s = self.state.read(cx);
            let running_conv = s
                .open_conversation
                .as_ref()
                .filter(|id| s.running_turn(id).is_some())
                .cloned();
            let reply_preview = s.reply_to.as_ref().and_then(|target_id| {
                let conv_id = s.open_conversation.as_ref()?;
                let target = s.entries(conv_id).iter().find(|e| &e.id == target_id);
                let who = match target.map(|e| &e.author) {
                    Some(Author::Owner) => "you".to_string(),
                    Some(Author::Bot { id }) => format!(
                        "@{}",
                        s.bot(id).map(|b| b.name.as_str()).unwrap_or(id.as_str())
                    ),
                    Some(Author::System) | None => "message".to_string(),
                };
                let snippet = target
                    .and_then(|e| e.kind.text())
                    .map(|t| {
                        let line = t.lines().next().unwrap_or("").trim();
                        if line.chars().count() > 64 {
                            let short: String = line.chars().take(64).collect();
                            format!("{short}…")
                        } else {
                            line.to_string()
                        }
                    })
                    .unwrap_or_default();
                Some((who, snippet))
            });
            (s.conn.is_online(), running_conv, reply_preview)
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
            .bg(rgb(theme::BACKGROUND))
            .px_4()
            .pb_4()
            .pt_1();

        // Mention picker, above the field.
        if let Some(picker) = &self.picker {
            let mut popup = div()
                .id("mention-popup")
                .w_full()
                .max_h(px(180.0))
                .overflow_y_scroll()
                .rounded_xl()
                .border_1()
                .border_color(rgb(theme::BORDER))
                .bg(rgb(0xffffff))
                .p_1()
                .flex()
                .flex_col()
                .gap_0p5();
            let matches = picker.matches.clone();
            for (i, option) in matches.iter().enumerate() {
                let selected = i == picker.selected;
                let (avatar_color, avatar_glyph, handle, subtitle) = match option {
                    MentionOption::Everyone => (
                        rgb(theme::ACCENT),
                        '@',
                        "@everyone".to_string(),
                        "Notify every member".to_string(),
                    ),
                    MentionOption::Bot(bot_id) => {
                        let bot = self.state.read(cx).bot(bot_id).cloned();
                        let (label, name, color) = match &bot {
                            Some(b) => (b.label.clone(), b.name.clone(), theme::bot_color(b)),
                            None => (
                                bot_id.as_str().to_string(),
                                bot_id.as_str().to_string(),
                                theme::bot_color_from_id(bot_id.as_str()),
                            ),
                        };
                        (color, bits::initial(&label), format!("@{name}"), label)
                    }
                };
                popup = popup.child(
                    div()
                        .id(SharedString::from(format!("mention-{i}")))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_lg()
                        .cursor_pointer()
                        .hover(|h| h.bg(rgb(theme::SURFACE)))
                        .when(selected, |d| d.bg(rgb(theme::SURFACE_RAISED)))
                        .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                            this.pick_mention(i, window, cx);
                        }))
                        .child(bits::avatar(avatar_color, avatar_glyph, px(20.0)))
                        .child(div().text_sm().text_color(rgb(theme::TEXT)).child(handle))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(subtitle),
                        ),
                );
            }
            body = body.child(popup);
        }

        // Reply target banner above the composer pill.
        if let Some((who, snippet)) = reply_preview {
            let label = if snippet.is_empty() {
                format!("Replying to {who}")
            } else {
                format!("Replying to {who}: {snippet}")
            };
            body = body.child(
                div()
                    .id("composer-reply-pill")
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::SURFACE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(label),
                    )
                    .child(
                        div()
                            .id("composer-reply-cancel")
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|h| h.bg(rgb(theme::SURFACE_RAISED)))
                            .text_xs()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child("✕")
                            .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                                this.state.update(cx, |s, cx| {
                                    s.reply_to = None;
                                    cx.notify();
                                });
                            })),
                    ),
            );
        }

        // Floating rounded pill bar matching screenshot
        let mut pill = div()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .px_2p5()
            .py_1p5()
            .rounded_full()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(0xffffff))
            .child(
                // Left circular + button
                div()
                    .size(px(30.0))
                    .flex_none()
                    .rounded_full()
                    .bg(rgb(0xf3f4f6))
                    .cursor_pointer()
                    .hover(|h| h.bg(rgb(0xe5e7eb)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .text_lg()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child("+"),
            )
            .child(
                // Center text input field
                div().flex_1().min_w_0().px_1().child(self.input.clone()),
            );

        if let Some(conv_id) = running_conv {
            pill = pill.child(
                div()
                    .id("composer-stop")
                    .px_2p5()
                    .py_1()
                    .flex_none()
                    .rounded_full()
                    .bg(rgb(0xfee2e2))
                    .border_1()
                    .border_color(rgb(theme::DANGER))
                    .cursor_pointer()
                    .hover(|h| h.bg(rgb(0xfecaca)))
                    .text_xs()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(rgb(theme::DANGER))
                    .child("Stop")
                    .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _, _| {
                        let _ = this.commands.send(Command::Interrupt(conv_id.clone()));
                    })),
            );
        }

        pill = pill.child(
            // Right circular send button
            div()
                .id("composer-send")
                .size(px(30.0))
                .flex_none()
                .rounded_full()
                .bg(rgb(0x000000))
                .cursor_pointer()
                .hover(|h| h.bg(rgb(0x222222)))
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(theme::BACKGROUND))
                .text_xs()
                .child(if online { "↑" } else { "✕" })
                .on_click(cx.listener(|this, _: &gpui::ClickEvent, _, cx| {
                    this.send_now(cx);
                })),
        );

        body.child(pill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::{Bot, Conversation, ConversationId};

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

        // Natural name matching without @
        let natural = addressed_bots("halo scout tolong carikan file", &members);
        assert_eq!(natural, vec![members[0].id.clone()]);

        let natural_multiple = addressed_bots("halo dev dan scout!", &members);
        assert_eq!(natural_multiple.len(), 2);
        assert!(
            natural_multiple.contains(&members[0].id) && natural_multiple.contains(&members[1].id)
        );

        // Word boundary avoids substrings
        assert!(addressed_bots("developer sedang bekerja", &members).is_empty());
    }

    #[test]
    fn match_mentions_includes_everyone_in_groups_only() {
        let members = vec![bot("scout", "Scout"), bot("editor", "Editor")];
        let in_group = match_mentions("e", &members, true);
        assert_eq!(in_group[0], MentionOption::Everyone);
        assert!(in_group.contains(&MentionOption::Bot(members[1].id.clone())));

        let in_dm = match_mentions("e", &members, false);
        assert!(!in_dm.contains(&MentionOption::Everyone));
        assert_eq!(in_dm, vec![MentionOption::Bot(members[1].id.clone())]);

        let all_group = match_mentions("", &members, true);
        assert_eq!(all_group.len(), 3);
        assert_eq!(all_group[0], MentionOption::Everyone);
    }

    #[test]
    fn composer_placeholder_follows_open_conversation() {
        let mut s = AppState::new("http://brain.test");
        assert_eq!(composer_placeholder(&s), "Select a conversation…");

        let scout = bot("scout", "Scout");
        let dm = Conversation::direct(scout.id.clone());
        let dm_id = dm.id.clone();
        s.upsert_bot(scout.clone());
        s.upsert_conversation(dm);
        s.open(dm_id);
        assert_eq!(composer_placeholder(&s), "Message @scout");

        let dev = bot("dev", "Dev");
        let group = Conversation::group(
            "Launch Team",
            vec![scout.id.clone(), dev.id.clone()],
            scout.id,
        )
        .expect("group");
        let gid = group.id.clone();
        s.upsert_bot(dev);
        s.upsert_conversation(group);
        s.open(gid);
        assert_eq!(composer_placeholder(&s), "Message #Launch Team");
    }

    #[test]
    fn has_at_everyone_detects_explicit_broadcast_mention() {
        assert!(has_at_everyone("@everyone hello"));
        assert!(has_at_everyone("cc @everyone and @scout"));
        assert!(!has_at_everyone("mail@everyone.com"));
        assert!(!has_at_everyone("hello everyone"));
    }
}

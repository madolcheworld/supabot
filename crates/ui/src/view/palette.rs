//! The command palette (`ctrl-k`): fuzzy-find a bot, conversation, task, routine
//! or screen and jump to it.
//!
//! The palette is its own focused overlay with a `"Palette"` key context, so its
//! arrows/enter navigate the results while the composer keeps its own bindings.
//! The result list uses [`gpui::uniform_list`] — the rows are single-line and
//! therefore uniform-height, which is exactly the case `uniform_list` is for.
//! Matching is a pure subsequence test ([`fuzzy`]).
//!
//! **The results follow the state, not just the keystrokes.** A palette that only
//! recomputed itself when the query changed would offer a bot that was deleted a
//! minute ago and hide one that just joined the roster — and since it is the
//! fastest way to jump anywhere, a stale row is a wrong jump. [`Palette::results_for`]
//! is a pure function of the state and the query; the entity recomputes it on
//! every state notification *and* every keystroke.

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
// The budget arithmetic and the shortening itself are one rule, shared with the
// sidebar's rows: both are `truncate()` in a flex row with no definite width, and
// a second copy would be a second thing to keep correct.
use crate::view::sidebar::{char_budget, ellipsize};
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

// ---------------------------------------------------------------------------
// Fitting a row's two texts inside the 560px card.
//
// A row used to be: title (`text_sm`, untruncated), a `flex_1` spacer, subtitle
// (`text_xs`, untruncated). A long bot title plus its subtitle was wider than the
// card's content box and **nothing clipped it** — the text painted over the
// popup's own rounded edge. The spacer is what made it invisible in review: it
// absorbed all the slack when the texts were short, so the layout looked correct
// until a real title arrived.
//
// `.min_w_0().truncate()` on both cells is the layout half of the fix (and the
// popup now clips, so nothing can paint outside its surface whatever a child
// does). The text half is [`fit_row_text`], because `.truncate()` alone is not
// reliable in a flex row: `TextLayout::layout` only ellipsizes when it is given a
// definite width, and a flex item's *content* is first measured under
// `AvailableSpace::MaxContent`. `nowrap` then makes the cached shape's
// `wrap_width` `None`, so every later pass reuses the untruncated size. The
// budget below is a conservative character count — approximate by construction,
// expensive to get exact, and honest about it.

/// The card is `560px`; a row's content box is that less the popup's `p_2` and
/// the row's `px_3`, the selection dot and the gaps: ~505px to share.
const ROW_CONTENT_W: f32 = 505.0;
/// How much of that a subtitle may take. Long subtitles are the "screen · …"
/// descriptions and task metadata; the title is the thing being chosen, so it
/// gets the rest.
const SUBTITLE_W: f32 = 240.0;
/// Deliberately wide average advances (the real ones are ~7.2 at 14px and ~6.0
/// at 12px), so the budget errs short and the `…` lands inside the card.
const TITLE_CHAR_W: f32 = 8.4;
const SUBTITLE_CHAR_W: f32 = 7.2;

/// Characters of a row title / subtitle that fit their columns, minus the slack
/// the conservative advances above already leave.
const TITLE_CHARS: usize = char_budget(ROW_CONTENT_W - SUBTITLE_W, TITLE_CHAR_W);
const SUBTITLE_CHARS: usize = char_budget(SUBTITLE_W, SUBTITLE_CHAR_W);

/// The title and subtitle a row actually shows, shortened to their budgets.
///
/// Pure so a test can hold it: this is the whole of "a long title does not paint
/// over the popup edge" that does not need a window.
fn fit_row_text(item: &PaletteItem) -> (String, String) {
    (
        ellipsize(&item.title, TITLE_CHARS),
        ellipsize(&item.subtitle, SUBTITLE_CHARS),
    )
}

pub struct Palette {
    input: Entity<TextInput>,
    state: Entity<AppState>,
    commands: Sender<Command>,
    results: Vec<PaletteItem>,
    selected: usize,
    /// Recompute on a keystroke.
    _observe_input: Subscription,
    /// Recompute on a state change: a bot upsert, a conversation deleted, a
    /// routine fetched all change what the palette can offer.
    _observe_state: Subscription,
}

/// The REST fetch a screen needs when the palette opens it.
///
/// Both the keyboard jump and the row click open screens, so the fetch lives in
/// one place: a screen that loads on one path and not the other is a bug that
/// shows up in only one of them.
fn fetch_for(screen: Screen, commands: &Sender<Command>, usage_hours: i64) {
    match screen {
        Screen::Conversation => {}
        Screen::Rules => {
            let _ = commands.send(Command::FetchRules);
        }
        Screen::Routines => {
            let _ = commands.send(Command::FetchRoutines);
        }
        Screen::Nodes => {
            let _ = commands.send(Command::FetchNodes);
        }
        Screen::Settings => {
            let _ = commands.send(Command::FetchSettings);
            let _ = commands.send(Command::FetchBackends);
        }
        Screen::Usage => {
            let _ = commands.send(Command::FetchUsage {
                since: Some(crate::view::usage::since_for(
                    usage_hours,
                    chrono::Utc::now(),
                )),
            });
        }
    }
}

impl Palette {
    pub fn new(state: Entity<AppState>, commands: Sender<Command>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Jump to a bot, conversation or task…", cx));
        let results = Self::results_for(state.read(cx), "");
        let observe_input = cx.observe(&input, |this, input, cx| {
            let query = input.read(cx).content().to_string();
            let s = this.state.read(cx);
            this.results = Self::results_for(s, &query);
            this.selected = 0;
            cx.notify();
        });
        // Live results: every reducer notification re-runs the same pure
        // function with the query that is currently typed, so a row can appear
        // or vanish without the owner touching the keyboard.
        let observe_state = cx.observe(&state, |this, _state, cx| {
            let query = this.input.read(cx).content().to_string();
            let s = this.state.read(cx);
            this.results = Self::results_for(s, &query);
            // The selection is an index into a list that just moved; keep it in
            // range rather than pointing at a row that no longer exists.
            if this.selected >= this.results.len() {
                this.selected = 0;
            }
            cx.notify();
        });
        Self {
            input,
            state,
            commands,
            results,
            selected: 0,
            _observe_input: observe_input,
            _observe_state: observe_state,
        }
    }

    pub fn focus_input(&self, window: &mut Window, cx: &App) {
        self.input.read(cx).focus(window);
    }

    /// Put the palette back to its opening state: an empty query and the first
    /// row selected.
    ///
    /// A palette is a *transient* surface — it appears, you jump, it goes away —
    /// so it must not remember the last query. It did: opening it again after
    /// jumping to "usage" showed `usage` already typed, and typing `nodes`
    /// produced the query `usagenodes`, which matches nothing. Measured on
    /// 2026-11-15 by driving the real window (ctrl-k, type, Enter, ctrl-k,
    /// type): the second query arrived concatenated. The stale text also left
    /// the *results* filtered by a query nobody could see the origin of.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.clear(cx));
        let s = self.state.read(cx);
        self.results = Self::results_for(s, "");
        self.selected = 0;
        cx.notify();
    }

    /// The palette's rows for one query over one state snapshot.
    ///
    /// Pure and public to the crate so a test can assert that a row appears when
    /// the state gains a bot and disappears when it loses one — the half of
    /// "live results" that does not need a window.
    pub(crate) fn results_for(s: &AppState, query: &str) -> Vec<PaletteItem> {
        let mut items = Vec::new();
        for bot in s.bots.iter() {
            items.push(PaletteItem {
                title: bot.label.clone(),
                subtitle: format!("@{} · bot", bot.name),
                action: PaletteAction::OpenBot(bot.id.clone()),
            });
        }
        for conversation in s.conversations.iter() {
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
        for routine in s.routines.iter() {
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
            title: "Usage".to_string(),
            subtitle: "screen · what the backends reported the team spent".to_string(),
            action: PaletteAction::OpenScreen(Screen::Usage),
        });
        items.push(PaletteItem {
            title: "Approval rules".to_string(),
            subtitle: "screen · saved answers that skip the card".to_string(),
            action: PaletteAction::OpenScreen(Screen::Rules),
        });
        items.push(PaletteItem {
            title: "Settings".to_string(),
            subtitle: "screen · brain configuration & backends".to_string(),
            action: PaletteAction::OpenScreen(Screen::Settings),
        });
        // A rule is a jump target too: the palette is the fastest way to reach a
        // permission the owner wants to withdraw, and its row names the screen it
        // lands on.
        for rule in s.approval_rules.iter() {
            items.push(PaletteItem {
                title: crate::view::rules::matcher_title(rule),
                subtitle: format!("{} · rule", crate::view::rules::effect_label(rule.effect)),
                action: PaletteAction::OpenScreen(Screen::Rules),
            });
        }
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
        if let PaletteAction::OpenScreen(screen) = action {
            fetch_for(screen, &self.commands, self.state.read(cx).usage_hours);
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
        let (title, subtitle) = fit_row_text(item);
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
            // `flex_1().min_w_0()` and not the old empty spacer: the title takes
            // the slack, so a long one shortens instead of shoving the subtitle
            // out of the card.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(rgb(theme::TEXT))
                    .truncate()
                    .child(title),
            )
            .child(
                div()
                    .flex_none()
                    .max_w(px(SUBTITLE_W))
                    .min_w_0()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_DIM))
                    .truncate()
                    .child(subtitle),
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
                if let PaletteAction::OpenScreen(screen) = action {
                    fetch_for(screen, &commands, state.read(cx).usage_hours);
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
            // Nothing a row draws may paint outside this surface: the rows used
            // to be wider than the card and there was no clip at all, so text ran
            // past the rounded edge and over whatever was behind the popup. This
            // is the guarantee; the per-row truncation above is what makes the
            // cut a `…` rather than a glyph.
            .overflow_hidden()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Fetched;
    use swarmbot_contracts::{Bot, Conversation, ConversationId};

    fn state_with_bots(names: &[&str]) -> AppState {
        let mut state = AppState::new("http://brain.test");
        let bots: Vec<Bot> = names
            .iter()
            .map(|name| Bot::new(*name, *name, "", "fake", ConversationId::new()))
            .collect();
        state.bots = Fetched::Ready(bots);
        state
    }

    fn titles(items: &[PaletteItem]) -> Vec<String> {
        items.iter().map(|item| item.title.clone()).collect()
    }

    /// The palette's rows are a pure function of the state and the query, which
    /// is what lets the entity recompute them on a live event and not only on a
    /// keystroke. Before this, a bot that joined (or left) after the palette was
    /// built was invisible (or still offered) until the owner typed something.
    #[test]
    fn the_rows_follow_the_state_and_not_only_the_query() {
        let mut state = state_with_bots(&["lead"]);
        let before = Palette::results_for(&state, "");
        assert!(titles(&before).contains(&"lead".to_string()));

        // A live `BotUpsert` changes the state; the same query now sees it.
        state.upsert_bot(Bot::new(
            "scout",
            "scout",
            "",
            "fake",
            ConversationId::new(),
        ));
        let after = Palette::results_for(&state, "");
        assert!(
            titles(&after).contains(&"scout".to_string()),
            "a bot that joined after the palette was built is offered: {:?}",
            titles(&after)
        );

        // …and one that left stops being offered.
        let gone = state.bots.as_slice()[0].id.clone();
        state.remove_bot(&gone);
        let after_delete = Palette::results_for(&state, "");
        assert!(
            !titles(&after_delete).contains(&"lead".to_string()),
            "a deleted bot is no longer a jump target: {:?}",
            titles(&after_delete)
        );
    }

    /// A conversation that arrives while the palette is open is findable by
    /// title, which is how a group created a second ago becomes reachable.
    #[test]
    fn a_new_conversation_is_findable_without_retyping() {
        let mut state = state_with_bots(&["lead"]);
        state.conversations = Fetched::Ready(Vec::new());
        assert!(Palette::results_for(&state, "launch").is_empty());

        let mut group = Conversation::group(
            "Launch plan",
            vec![state.bots.as_slice()[0].id.clone(), BotId::new()],
            state.bots.as_slice()[0].id.clone(),
        )
        .expect("group");
        group.title = Some("Launch plan".into());
        state.upsert_conversation(group);

        let found = Palette::results_for(&state, "launch");
        assert!(
            found.iter().any(|item| item.title == "Launch plan"
                && matches!(item.action, PaletteAction::OpenConversation(_))),
            "{:?}",
            titles(&found)
        );
    }

    /// A rule is a jump target: the palette is the fastest route to a saved
    /// "always allow" the owner wants to see or withdraw.
    #[test]
    fn saved_rules_are_offered_as_jump_targets() {
        let mut state = state_with_bots(&["lead"]);
        let mut rule = swarmbot_contracts::ApprovalRule::new(
            swarmbot_contracts::RuleScope::Global,
            swarmbot_contracts::RuleEffect::Allow,
        );
        rule.tool = Some("shell".into());
        state.approval_rules = Fetched::Ready(vec![rule]);

        let found = Palette::results_for(&state, "shell");
        assert!(
            found.iter().any(|item| {
                item.action == PaletteAction::OpenScreen(Screen::Rules)
                    && item.title == "tool `shell`"
                    && item.subtitle.starts_with("allow")
            }),
            "{:?}",
            titles(&found)
        );
        // The screen itself is always offered too.
        assert!(
            titles(&Palette::results_for(&state, "rules"))
                .iter()
                .any(|title| title == "Approval rules")
        );
    }

    // -----------------------------------------------------------------------
    // Defect: a long bot/conversation title plus its subtitle was wider than the
    // 560px card with no truncation and no clip, so the text painted over the
    // popup's rounded edge. The tests here hold the character budget and the
    // budget arithmetic; the layout half (`.min_w_0().truncate()`, `.max_w(..)`,
    // `overflow_hidden()`) is not something a headless test can see, and this
    // file does not pretend otherwise.
    // -----------------------------------------------------------------------

    fn item(title: &str, subtitle: &str) -> PaletteItem {
        PaletteItem {
            title: title.to_string(),
            subtitle: subtitle.to_string(),
            action: PaletteAction::OpenScreen(Screen::Usage),
        }
    }

    /// A long title and a long subtitle both come back shortened with `…`, and
    /// neither exceeds its column budget — which is what stops the row being
    /// wider than the card in the first place.
    #[test]
    fn a_long_title_and_subtitle_are_both_shortened_with_an_ellipsis() {
        let long = item(
            "the deploy bot that watches the staging cluster all night",
            "screen · what the backends reported the team spent across every node",
        );
        let (title, subtitle) = fit_row_text(&long);
        assert!(
            title.ends_with('…') && title.chars().count() <= TITLE_CHARS,
            "a {}-char title in a ~265px column must end in an ellipsis: {title:?}",
            long.title.chars().count()
        );
        assert!(
            subtitle.ends_with('…') && subtitle.chars().count() <= SUBTITLE_CHARS,
            "a {}-char subtitle in a ~240px column must end in an ellipsis: {subtitle:?}",
            long.subtitle.chars().count()
        );
        // Together they fit the row's content box, with room for the dot and the
        // three `gap_2`s — the row is not wider than the card any more.
        let estimated: f32 = title.chars().count() as f32 * TITLE_CHAR_W
            + subtitle.chars().count() as f32 * SUBTITLE_CHAR_W;
        assert!(
            estimated <= ROW_CONTENT_W,
            "the fitted cells are {estimated:.0}px of a {ROW_CONTENT_W}px row"
        );
    }

    /// Short text is left exactly as the state gave it — the budget must not
    /// rewrite ordinary rows, because the palette shows them verbatim.
    #[test]
    fn ordinary_rows_are_not_rewritten() {
        let (title, subtitle) = fit_row_text(&item("scout", "@scout · bot"));
        assert_eq!(title, "scout");
        assert_eq!(subtitle, "@scout · bot");
    }

    /// The budgets are believable for the card, and they only shorten a row that
    /// genuinely does not fit: nothing is rewritten just because a limit exists.
    #[test]
    fn the_budgets_fit_the_card_and_only_shorten_rows_that_overflow() {
        const {
            assert!(TITLE_CHARS > 0 && SUBTITLE_CHARS > 0);
            // Even "group · conversation" is 20 chars; a narrower budget would
            // shorten ordinary rows rather than overflowing ones.
            assert!(SUBTITLE_CHARS >= 20);
        }
        // The two columns plus the dot and gaps must fit the card's content box,
        // or the row is wider than the popup again.
        let columns = TITLE_CHARS as f32 * TITLE_CHAR_W + SUBTITLE_CHARS as f32 * SUBTITLE_CHAR_W;
        assert!(
            columns <= ROW_CONTENT_W,
            "the two columns want {columns:.0}px of a {ROW_CONTENT_W}px row"
        );

        // Only a cell that is genuinely over budget may be rewritten; one that
        // fits must reach the screen character-for-character.
        let check = |text: &str, fitted: &str, budget: usize, what: &str| {
            if fitted == text {
                assert!(
                    text.chars().count() <= budget,
                    "{what} {text:?} fits in {budget} chars but was passed through unchanged"
                );
            } else {
                assert!(
                    text.chars().count() > budget,
                    "{what} {text:?} fits in {budget} chars but was shortened to {fitted:?}"
                );
                assert!(
                    fitted.ends_with('…'),
                    "{what} was shortened without an ellipsis: {fitted:?}"
                );
            }
        };
        let state = state_with_bots(&["lead"]);
        for row in Palette::results_for(&state, "") {
            let (title, subtitle) = fit_row_text(&row);
            check(&row.title, &title, TITLE_CHARS, "title");
            check(&row.subtitle, &subtitle, SUBTITLE_CHARS, "subtitle");
        }
    }
}

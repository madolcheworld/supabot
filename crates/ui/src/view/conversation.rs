//! The conversation view: a header (title, members, a live "Handling…" line and
//! Stop) and one transcript row per [`Entry`].
//!
//! The row renderer is the visible half of the mapping rules in
//! [`../../../docs/ui.md`] §3: owner bubbles right, bot bubbles left in the
//! author's colour, `reasoning` dim and collapsed, `tool_call` a collapsible
//! card, `event` lines centred and dim (these are the delegation lines), a
//! `card` inline, `notice` coloured by level, and `Unknown` a placeholder rather
//! than a crash.

use std::collections::HashMap;
use std::sync::mpsc::Sender;

use gpui::{AnyElement, Entity, SharedString, div, prelude::*, px, rgb};
use swarmbot_contracts::{
    Author, Bot, Conversation, Entry, EntryId, EntryKind, NoticeLevel, PostState, ToolCallStatus,
};

use crate::proto::Command;
use crate::state::AppState;
use crate::theme;
use crate::view::cards::{self, CardView};
use crate::widgets::bits::{self, Tone};
use crate::widgets::markdown;
use crate::widgets::text_input::TextInput;

/// The conversation header: title, member tiles, the live activity line, Stop,
/// and — for a DM — the provider button that opens [`super::provider_menu`].
pub fn header(
    s: &AppState,
    state: &Entity<AppState>,
    model_input: &Entity<TextInput>,
    commands: &Sender<Command>,
) -> gpui::Div {
    let Some(conversation) = s.open_conversation() else {
        return div()
            .h(theme::space::HEADER_H)
            .flex()
            .items_center()
            .px_4()
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .text_color(rgb(theme::TEXT_DIM))
            .child("No conversation");
    };
    let title = s.conversation_title(conversation);
    let conv_id = conversation.id.clone();
    let members = s.members(&conv_id);

    // Member avatar tiles.
    let mut tiles = div().flex().items_center().gap_1();
    for bot_id in &members {
        if let Some(bot) = s.bot(bot_id) {
            let bot_id_c = bot.id.clone();
            let bot_backend_c = bot.backend.clone();
            let bot_model_c = bot.model.clone();
            let state_h = state.clone();
            let input_h = model_input.clone();
            let commands_menu = commands.clone();
            let is_menu_active = s.bot_menu_open && s.menu_bot.as_ref() == Some(&bot.id);
            tiles = tiles.child(
                div()
                    .id(SharedString::from(format!("member-tile-{}", bot.id)))
                    .cursor_pointer()
                    .when(is_menu_active, |d| {
                        d.rounded_md().border_1().border_color(rgb(theme::ACCENT))
                    })
                    .on_click(move |_, _, cx: &mut gpui::App| {
                        let mut fetch: Option<String> = None;
                        state_h.update(cx, |s, cx| {
                            let was_this_bot =
                                s.bot_menu_open && s.menu_bot.as_ref() == Some(&bot_id_c);
                            if was_this_bot {
                                s.bot_menu_open = false;
                                s.menu_bot = None;
                            } else {
                                s.bot_menu_open = true;
                                s.menu_bot = Some(bot_id_c.clone());
                                s.menu_backend = Some(bot_backend_c.clone());
                                s.menu_model = bot_model_c.clone();
                                if !s.models.contains_key(&bot_backend_c) {
                                    fetch = Some(bot_backend_c.clone());
                                }
                            }
                            cx.notify();
                        });
                        let initial = bot_model_c.clone().unwrap_or_default();
                        input_h.update(cx, |input, cx| input.set_content(&initial, cx));
                        let _ = commands_menu.send(Command::FetchBackends);
                        if let Some(backend) = fetch {
                            let _ = commands_menu.send(Command::FetchModels(backend));
                        }
                    })
                    .child(bits::avatar(
                        theme::bot_color(bot),
                        bits::initial(&bot.label),
                        px(20.0),
                    )),
            );
        }
    }

    // The live "Handling…" line, from a running turn in this conversation.
    let running = s.running_turn(&conv_id).is_some();
    let activity = s
        .handling_bot(&conv_id)
        .and_then(|b| match &b.status {
            swarmbot_contracts::BotStatus::Working { activity, .. } => Some(activity.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "Handling…".to_string());

    // Stop button: meaningful only while a turn runs and we are connected.
    let can_stop = running && s.conn.is_online();
    let stop = if can_stop {
        let commands = commands.clone();
        let conv = conv_id.clone();
        bits::button("Stop", Tone::Danger)
            .id(SharedString::from(format!("stop-{conv_id}")))
            .on_click(move |_, _, _| {
                let _ = commands.send(Command::Interrupt(conv.clone()));
            })
            .into_any_element()
    } else {
        bits::disabled_button("Stop").into_any_element()
    };

    let direct_bot = dm_bot(s, conversation);
    let (bot_label, bot_color, bot_initial) = if let Some(bot) = direct_bot.as_ref() {
        (
            bot.label.clone(),
            theme::bot_color(bot),
            bits::initial(&bot.label),
        )
    } else {
        (title.clone(), theme::hex("#8b5cf6"), 'W')
    };

    let direct_pill = direct_bot.as_ref().map(|bot| {
        let state_h = state.clone();
        let input_h = model_input.clone();
        let commands_menu = commands.clone();
        let menu_bot_id = bot.id.clone();
        let menu_backend = bot.backend.clone();
        let current_model = bot.model.clone();

        let model_display = match &bot.model {
            Some(model) => format!("{model} ▾"),
            None => format!("{} ▾", bot.backend),
        };
        let is_open_for_this =
            s.bot_menu_open && s.menu_bot.as_ref().is_none_or(|id| id == &bot.id);

        div()
            .id("provider-menu-toggle")
            .cursor_pointer()
            .flex()
            .items_center()
            .px_2()
            .py(px(1.5))
            .rounded_full()
            .border_1()
            .text_xs()
            .font_weight(gpui::FontWeight::MEDIUM)
            .when(is_open_for_this, |d| {
                d.bg(rgb(0xede9fe))
                    .border_color(rgb(0xc4b5fd))
                    .text_color(rgb(0x6d28d9))
            })
            .when(!is_open_for_this, |d| {
                d.bg(rgb(0xf3f4f6))
                    .border_color(rgb(0xe5e7eb))
                    .text_color(rgb(0x374151))
                    .hover(|h| h.bg(rgb(0xe5e7eb)))
            })
            .on_click(move |_, _, cx: &mut gpui::App| {
                let mut opening = false;
                let mut fetch: Option<String> = None;
                state_h.update(cx, |s, cx| {
                    let was_open =
                        s.bot_menu_open && s.menu_bot.as_ref().is_none_or(|id| id == &menu_bot_id);
                    s.bot_menu_open = !was_open;
                    opening = s.bot_menu_open;
                    if opening {
                        s.menu_bot = Some(menu_bot_id.clone());
                        s.menu_backend = Some(menu_backend.clone());
                        s.menu_model = current_model.clone();
                        if !s.models.contains_key(&menu_backend) {
                            fetch = Some(menu_backend.clone());
                        }
                    } else {
                        s.menu_bot = None;
                    }
                    cx.notify();
                });
                if opening {
                    let initial_text = current_model.clone().unwrap_or_default();
                    input_h.update(cx, |input, cx| input.set_content(&initial_text, cx));
                    let _ = commands_menu.send(Command::FetchBackends);
                    if let Some(backend) = fetch {
                        let _ = commands_menu.send(Command::FetchModels(backend));
                    }
                }
            })
            .child(model_display)
    });

    let group_pill = if direct_bot.is_none() && !members.is_empty() {
        let target_bot_id = s.menu_bot.as_ref().or_else(|| match &conversation.kind {
            swarmbot_contracts::ConversationKind::Group { lead, .. } => Some(lead),
            _ => members.first(),
        });
        target_bot_id.and_then(|id| s.bot(id)).map(|bot| {
            let state_h = state.clone();
            let input_h = model_input.clone();
            let commands_menu = commands.clone();
            let menu_bot_id = bot.id.clone();
            let menu_backend = bot.backend.clone();
            let current_model = bot.model.clone();

            let model_display = match &bot.model {
                Some(model) => format!("{}: {model} ▾", bot.name),
                None => format!("{}: {} ▾", bot.name, bot.backend),
            };
            let is_open_for_this = s.bot_menu_open && s.menu_bot.as_ref() == Some(&bot.id);

            div()
                .id("group-provider-menu-toggle")
                .cursor_pointer()
                .flex()
                .items_center()
                .px_2()
                .py(px(1.5))
                .rounded_full()
                .border_1()
                .text_xs()
                .font_weight(gpui::FontWeight::MEDIUM)
                .when(is_open_for_this, |d| {
                    d.bg(rgb(0xede9fe))
                        .border_color(rgb(0xc4b5fd))
                        .text_color(rgb(0x6d28d9))
                })
                .when(!is_open_for_this, |d| {
                    d.bg(rgb(0xf3f4f6))
                        .border_color(rgb(0xe5e7eb))
                        .text_color(rgb(0x374151))
                        .hover(|h| h.bg(rgb(0xe5e7eb)))
                })
                .on_click(move |_, _, cx: &mut gpui::App| {
                    let mut opening = false;
                    let mut fetch: Option<String> = None;
                    state_h.update(cx, |s, cx| {
                        let was_open = s.bot_menu_open && s.menu_bot.as_ref() == Some(&menu_bot_id);
                        s.bot_menu_open = !was_open;
                        opening = s.bot_menu_open;
                        if opening {
                            s.menu_bot = Some(menu_bot_id.clone());
                            s.menu_backend = Some(menu_backend.clone());
                            s.menu_model = current_model.clone();
                            if !s.models.contains_key(&menu_backend) {
                                fetch = Some(menu_backend.clone());
                            }
                        } else {
                            s.menu_bot = None;
                        }
                        cx.notify();
                    });
                    if opening {
                        let initial_text = current_model.clone().unwrap_or_default();
                        input_h.update(cx, |input, cx| input.set_content(&initial_text, cx));
                        let _ = commands_menu.send(Command::FetchBackends);
                        if let Some(backend) = fetch {
                            let _ = commands_menu.send(Command::FetchModels(backend));
                        }
                    }
                })
                .child(model_display)
        })
    } else {
        None
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
        .bg(rgb(0xffffff))
        .child(bits::avatar(bot_color, bot_initial, px(32.0)))
        .child(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_base()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(theme::TEXT))
                                .truncate()
                                .child(bot_label),
                        )
                        .children(direct_pill)
                        .children(group_pill),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(if running {
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .truncate()
                                .child(bits::spinner(s.spinner_phase))
                                .child(activity)
                                .into_any_element()
                        } else if let Some(bot) = &direct_bot {
                            div()
                                .truncate()
                                .child(format!("@{} · direct conversation", bot.name))
                                .into_any_element()
                        } else {
                            div()
                                .truncate()
                                .child(format!("{} members", members.len()))
                                .into_any_element()
                        }),
                ),
        )
        .child(div().flex_1())
        .child(tiles)
        .child(stop)
}

/// The single bot of a direct conversation, when this is one. The provider
/// menu configures exactly one bot; a group has several with possibly
/// different backends, and one dropdown cannot honestly speak for all.
pub fn dm_bot(s: &AppState, conversation: &Conversation) -> Option<Bot> {
    match &conversation.kind {
        swarmbot_contracts::ConversationKind::Direct { bot_id } => s.bot(bot_id).cloned(),
        _ => None,
    }
}

/// Render one transcript entry as a row. `prev` is the entry before it (when
/// any): consecutive messages by the same author group visually — the repeated
/// name/time header is dropped and the bubbles tighten up.
pub fn entry_row(
    s: &AppState,
    state: &Entity<AppState>,
    _commands: &Sender<Command>,
    cards: &HashMap<EntryId, Entity<CardView>>,
    entry: &Entry,
    prev: Option<&Entry>,
    is_last: bool,
) -> AnyElement {
    match &entry.kind {
        EntryKind::Message(message) => message_row(s, entry, message, is_last, prev),
        EntryKind::Reasoning(reasoning) => div()
            .w_full()
            .py_1()
            .pl(px(AVATAR_COL))
            .child(collapsible_row(
                s,
                state,
                entry,
                "Reasoning",
                theme::TEXT_DIM,
                &reasoning.text,
                true,
            ))
            .into_any_element(),
        EntryKind::ToolCall(call) => div()
            .w_full()
            .py_1()
            .pl(px(AVATAR_COL))
            .child(tool_row(s, state, entry, call))
            .into_any_element(),
        EntryKind::Card(card) => match cards.get(&entry.id) {
            Some(view) => div().w_full().py_1().child(view.clone()).into_any_element(),
            None => cards::placeholder(&card.request_id).into_any_element(),
        },
        EntryKind::Event(event) => div()
            .w_full()
            .py_1()
            .flex()
            .justify_center()
            .child(
                div()
                    .text_xs()
                    .italic()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child(event.text.clone()),
            )
            .into_any_element(),
        EntryKind::Notice(notice) => {
            if notice.level == NoticeLevel::Error {
                div()
                    .w_full()
                    .py_2()
                    .child(render_bot_failed_banner(Some(&notice.text)))
                    .into_any_element()
            } else {
                let color = match notice.level {
                    NoticeLevel::Info => theme::TEXT_MUTED,
                    NoticeLevel::Warning => theme::WARNING,
                    NoticeLevel::Error => theme::DANGER,
                };
                div()
                    .w_full()
                    .py_1()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(color))
                            .child(notice.text.clone()),
                    )
                    .into_any_element()
            }
        }
        EntryKind::Unknown => div()
            .w_full()
            .py_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("[an entry kind this build does not know]"),
            )
            .into_any_element(),
    }
    .into_any_element()
}

/// Avatar column width (24px avatar + 8px gap): bot bubbles start here, and
/// reasoning/tool rows indent to the same line so the transcript has one left
/// edge.
const AVATAR_COL: f32 = 32.0;

/// True when `entry` continues `prev`: both plain messages, same author, less
/// than five minutes apart. Grouped messages drop the repeated header — the
/// transcript should read as a conversation, not a stack of cards. Pure so a
/// test can pin the rule.
pub fn same_group(prev: Option<&Entry>, entry: &Entry) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    if !matches!(prev.kind, EntryKind::Message(_)) || !matches!(entry.kind, EntryKind::Message(_)) {
        return false;
    }
    if prev.author != entry.author {
        return false;
    }
    let gap = entry.created_at.signed_duration_since(prev.created_at);
    (0..300).contains(&gap.num_seconds())
}

fn message_row(
    s: &AppState,
    entry: &Entry,
    message: &swarmbot_contracts::MessageEntry,
    is_last: bool,
    prev: Option<&Entry>,
) -> AnyElement {
    let owner = entry.author.is_owner();
    let (label, color) = match &entry.author {
        Author::Owner => ("You".to_string(), rgb(theme::ACCENT)),
        Author::Bot { id } => {
            let bot = s.bot(id);
            (
                bot.map(|b| b.label.clone())
                    .unwrap_or_else(|| id.as_str().to_string()),
                bot.map(theme::bot_color)
                    .unwrap_or_else(|| theme::bot_color_from_id(id.as_str())),
            )
        }
        Author::System => ("system".to_string(), rgb(theme::TEXT_DIM)),
    };

    let streaming = is_last
        && !owner
        && s.open_conversation().is_some_and(|c| {
            s.running_turn(&c.id)
                .is_some_and(|t| matches!(&entry.author, Author::Bot { id } if *id == t.bot_id))
        });

    let grouped = same_group(prev, entry);

    // A streamed entry is committed empty and grows with deltas; until the
    // first one lands (an ACP turn can take seconds: session setup, then the
    // agent's first token) a bare bubble reads as a bug. Show the caret while
    // the turn runs, and a dim ellipsis in the (should-not-happen) case of a
    // final empty message.
    let mut body = if message.text.is_empty() {
        div().child(
            div()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(if streaming { "▋" } else { "…" }),
        )
    } else {
        markdown::render_markdown(&message.text)
    };
    if streaming && !message.text.is_empty() {
        body = body.child(div().text_color(rgb(theme::TEXT_MUTED)).child("▋"));
    }

    let mut bubble = div()
        .max_w(px(520.0))
        .min_w_0()
        .overflow_hidden()
        .px_4()
        .py_2()
        .bg(rgb(if owner {
            theme::OWNER_BUBBLE
        } else {
            theme::BOT_BUBBLE
        }))
        .text_color(rgb(if owner { 0xffffff } else { theme::TEXT }))
        .when(owner, |d| d.rounded_full())
        .when(!owner, |d| {
            d.rounded_2xl().border_1().border_color(rgb(theme::BORDER))
        });

    // The name/time header opens a group; for bot message only (owner message is clean black pill)
    if !grouped && !owner {
        bubble = bubble.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .mb_1()
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(color)
                        .child(label.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(entry.created_at.format("%H:%M").to_string()),
                )
                .when(message.post == PostState::Queued, |d| {
                    d.child(bits::badge("queued", theme::WARNING, 0x1a1400))
                })
                .when(message.post == PostState::Steered, |d| {
                    d.child(bits::badge("steered", theme::ACCENT, 0x0a1428))
                }),
        );
    }
    bubble = bubble.child(body);

    let mut outer = div().w_full().flex().flex_col();

    // Centered timestamp separator when starting a new message group
    if !grouped {
        outer = outer.child(
            div()
                .w_full()
                .flex()
                .justify_center()
                .py_2()
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .child(entry.created_at.format("Today %-I:%M %p").to_string()),
        );
    }

    // One left edge: bot rows carry the avatar; owner bubbles hug the right.
    let mut row = div()
        .w_full()
        .flex()
        .gap_2()
        .pb(px(2.0))
        .when(grouped, |d| d.pt(px(1.0)));
    if owner {
        row = row.justify_end().child(bubble);
    } else {
        let gutter = if grouped {
            div().w(px(28.0)).h(px(1.0)).flex_none()
        } else {
            bits::avatar(color, bits::initial(&label), px(28.0))
        };
        row = row.child(gutter).child(bubble);
    }
    outer.child(row).into_any_element()
}

/// A dim, collapsible block (used for reasoning).
fn collapsible_row(
    s: &AppState,
    state: &Entity<AppState>,
    entry: &Entry,
    label: &str,
    color: u32,
    text: &str,
    collapsed_by_default: bool,
) -> AnyElement {
    let expanded = s.is_expanded(&entry.id);
    let _ = collapsed_by_default;
    let id = entry.id.clone();
    let state_h = state.clone();
    let toggle_id = SharedString::from(format!("tgl-{}", entry.id));
    let mut block = div()
        .w_full()
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .id(toggle_id)
                .flex()
                .items_center()
                .gap_2()
                .cursor_pointer()
                .text_xs()
                .text_color(rgb(color))
                .child(if expanded { "▾" } else { "▸" })
                .child(label.to_string())
                .on_click(move |_, _, cx: &mut gpui::App| {
                    state_h.update(cx, |st, cx| {
                        st.toggle_expanded(&id);
                        cx.notify();
                    });
                }),
        );
    if expanded {
        block = block.child(
            div()
                .w_full()
                .pl_4()
                .text_sm()
                .italic()
                .text_color(rgb(color))
                .child(text.to_string()),
        );
    }
    block.into_any_element()
}

/// A tool-call card: name, status, duration, node; expandable args + result.
fn tool_row(
    s: &AppState,
    state: &Entity<AppState>,
    entry: &Entry,
    call: &swarmbot_contracts::ToolCallEntry,
) -> AnyElement {
    let expanded = s.is_expanded(&entry.id);
    let errored = matches!(
        call.status,
        ToolCallStatus::Failed | ToolCallStatus::Denied | ToolCallStatus::Cancelled
    );
    let accent = if errored {
        theme::DANGER
    } else {
        match call.status {
            ToolCallStatus::Completed => theme::SUCCESS,
            ToolCallStatus::Running | ToolCallStatus::Pending => theme::ACCENT,
            _ => theme::TEXT_DIM,
        }
    };

    let id = entry.id.clone();
    let state_h = state.clone();
    let toggle_id = SharedString::from(format!("tool-{}", entry.id));

    let mut meta = div()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .text_color(rgb(theme::TEXT_DIM))
        .child(call.tool.clone())
        .child(bits::dot(accent, px(7.0)))
        .child(call.status.status_label());
    if let Some(ms) = call.duration_ms {
        meta = meta.child(format!("· {ms} ms"));
    }
    if let Some(node) = &call.node_id {
        meta = meta.child(format!("· {}", node));
    }

    let mut card = div()
        .w_full()
        .rounded_md()
        .border_1()
        .border_color(rgb(if errored {
            theme::DANGER
        } else {
            theme::BORDER
        }))
        .bg(rgb(theme::SURFACE))
        .overflow_hidden()
        .child(
            div()
                .id(toggle_id)
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .cursor_pointer()
                .child(div().text_xs().text_color(rgb(accent)).child(if expanded {
                    "▾"
                } else {
                    "▸"
                }))
                .child(meta)
                .on_click(move |_, _, cx: &mut gpui::App| {
                    state_h.update(cx, |st, cx| {
                        st.toggle_expanded(&id);
                        cx.notify();
                    });
                }),
        );

    if expanded {
        let args =
            serde_json::to_string_pretty(&call.args).unwrap_or_else(|_| call.args.to_string());
        card = card.child(
            div()
                .px_2()
                .pb_1()
                .flex()
                .flex_col()
                .gap_1()
                .child(code_block(
                    SharedString::from(format!("args-{}", entry.id)),
                    "arguments",
                    &args,
                ))
                .children(call.result_text.as_ref().map(|result| {
                    let full = s.is_full(&entry.id);
                    let truncated = !full && result.chars().count() > 400;
                    let shown: String = if truncated {
                        result.chars().take(400).collect()
                    } else {
                        result.clone()
                    };
                    let mut col = div().flex().flex_col().gap_1().child(code_block(
                        SharedString::from(format!("res-{}", entry.id)),
                        if errored { "result (error)" } else { "result" },
                        &shown,
                    ));
                    if let Some(spill) = &call.spill_path {
                        col = col.child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_DIM))
                                .child(format!("full output spilled to {spill}")),
                        );
                    }
                    if truncated || full {
                        let id2 = entry.id.clone();
                        let state_h2 = state.clone();
                        col = col.child(
                            div()
                                .id(SharedString::from(format!("more-{}", entry.id)))
                                .text_xs()
                                .text_color(rgb(theme::ACCENT_HOVER))
                                .cursor_pointer()
                                .child(if full { "show less" } else { "show more" })
                                .on_click(move |_, _, cx: &mut gpui::App| {
                                    state_h2.update(cx, |st, cx| {
                                        st.toggle_full(&id2);
                                        cx.notify();
                                    });
                                }),
                        );
                    }
                    col
                })),
        );
    }

    card.into_any_element()
}

fn code_block(id: impl Into<gpui::ElementId>, label: &str, text: &str) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .child(label.to_string()),
        )
        .child(
            div()
                .id(id)
                .w_full()
                .rounded_sm()
                .bg(rgb(theme::CODE_BG))
                .px_2()
                .py_1()
                .font_family("DejaVu Sans Mono")
                .text_size(px(12.0))
                .text_color(rgb(theme::TEXT))
                .overflow_x_scroll()
                .child(text.to_string()),
        )
}

/// A short label for a tool-call status (the contracts expose `is_terminal` but
/// not a label, so this keeps the wording in the UI).
trait StatusLabel {
    fn status_label(&self) -> &'static str;
}
impl StatusLabel for ToolCallStatus {
    fn status_label(&self) -> &'static str {
        match self {
            ToolCallStatus::Pending => "pending",
            ToolCallStatus::Running => "running",
            ToolCallStatus::Completed => "completed",
            ToolCallStatus::Failed => "failed",
            ToolCallStatus::Denied => "denied",
            ToolCallStatus::Cancelled => "cancelled",
        }
    }
}

/// A simulated conversation history matching the screenshot when a conversation has no entries.
pub fn mock_conversation_view() -> gpui::Stateful<gpui::Div> {
    div()
        .id("mock-transcript-scroll")
        .flex_1()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap_2()
        .px_4()
        .py_2()
        // Yesterday 1:55 PM
        .child(centered_time_str("Yesterday 1:55 PM"))
        .child(user_pill_str("Hello"))
        // Today 5:34 AM
        .child(centered_time_str("Today 5:34 AM"))
        .child(user_pill_str("Hello"))
        .child(user_pill_str("Hello"))
        .child(user_pill_str("Hello"))
        // Today 7:38 AM
        .child(centered_time_str("Today 7:38 AM"))
        .child(user_pill_str("Hi 👋"))
        // Today 11:40 AM
        .child(centered_time_str("Today 11:40 AM"))
        .child(user_pill_str("hello"))
        // Error banner at bottom
        .child(render_bot_failed_banner(None))
}

fn centered_time_str(text: &str) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .justify_center()
        .py_3()
        .text_xs()
        .text_color(rgb(theme::TEXT_DIM))
        .child(text.to_string())
}

fn user_pill_str(text: &str) -> gpui::Div {
    div().w_full().flex().justify_end().py_1().child(
        div()
            .px_4()
            .py_2()
            .rounded_full()
            .bg(rgb(0x000000))
            .text_color(rgb(0xffffff))
            .text_sm()
            .child(text.to_string()),
    )
}

/// The soft-red warning banner shown when a bot fails to respond, matching the screenshot.
pub fn render_bot_failed_banner(detail: Option<&str>) -> gpui::Div {
    let subtitle = detail.unwrap_or("Something went wrong while generating the reply. Try again.");
    div()
        .w_full()
        .my_2()
        .p_3p5()
        .rounded_2xl()
        .bg(rgb(theme::NOTICE_ERROR_BG))
        .border_1()
        .border_color(rgb(theme::NOTICE_ERROR_BORDER))
        .flex()
        .items_start()
        .gap_3()
        .child(div().text_base().text_color(rgb(theme::DANGER)).child("⚠️"))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(theme::TEXT))
                        .child("Bot failed to respond"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(subtitle.to_string()),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(div().cursor_pointer().child("⛶"))
                .child(div().cursor_pointer().child("✕")),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::{Author, BotId, ConversationId, Entry, EntryKind, MessageEntry};

    fn message(text: &str) -> EntryKind {
        EntryKind::Message(Box::new(MessageEntry {
            text: text.into(),
            attachments: Vec::new(),
            reply_to: None,
            post: PostState::Posted,
            addressed_to: None,
            turn_id: None,
        }))
    }

    fn entry_at(author: Author, kind: EntryKind, secs: i64) -> Entry {
        let mut entry = Entry::new(ConversationId::new(), 1, author, kind);
        entry.created_at = chrono::DateTime::from_timestamp(secs, 0).expect("a valid timestamp");
        entry
    }

    fn bot_author() -> Author {
        Author::Bot { id: BotId::new() }
    }

    #[test]
    fn consecutive_messages_by_one_author_group() {
        let author = bot_author();
        let first = entry_at(author.clone(), message("one"), 1_000);
        let second = entry_at(author.clone(), message("two"), 1_060);
        assert!(same_group(Some(&first), &second));
        // The first message of a run never groups (there is nothing before it).
        assert!(!same_group(None, &first));
    }

    #[test]
    fn a_new_author_or_a_gap_breaks_the_group() {
        let a = bot_author();
        let b = bot_author();
        let first = entry_at(a, message("one"), 1_000);
        let other_author = entry_at(b, message("two"), 1_010);
        assert!(!same_group(Some(&first), &other_author));
        let late = entry_at(first.author.clone(), message("three"), 1_000 + 301);
        assert!(
            !same_group(Some(&first), &late),
            "five minutes of silence starts a new group"
        );
        let owner_reply = entry_at(Author::Owner, message("four"), 1_010);
        assert!(!same_group(Some(&first), &owner_reply));
    }

    #[test]
    fn the_provider_menu_only_applies_to_a_direct_conversation() {
        let mut state = AppState::new("http://127.0.0.1:0".to_string());
        let bot =
            swarmbot_contracts::Bot::new("codie", "Codie", "", "codex", ConversationId::new());
        let bot_id = bot.id.clone();
        state.bots.push(bot);
        let dm = Conversation::direct(bot_id.clone());
        assert_eq!(
            dm_bot(&state, &dm).map(|b| b.id),
            Some(bot_id.clone()),
            "a DM has exactly one configurable bot"
        );
        let group = Conversation::group("Team", vec![bot_id.clone(), BotId::new()], bot_id)
            .expect("a valid group");
        assert!(
            dm_bot(&state, &group).is_none(),
            "a group has no single bot to configure"
        );
    }

    #[test]
    fn non_message_entries_never_group() {
        let author = bot_author();
        let first = entry_at(author.clone(), message("one"), 1_000);
        let event = entry_at(
            author.clone(),
            EntryKind::Event(Box::new(swarmbot_contracts::EventEntry {
                event: "x".into(),
                text: "y".into(),
                data: serde_json::Value::Null,
            })),
            1_010,
        );
        // A message after an event is a fresh block…
        assert!(!same_group(Some(&event), &first));
        // …and an event after a message never groups either.
        assert!(!same_group(Some(&first), &event));
    }
}

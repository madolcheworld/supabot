//! The left sidebar: the app's title row, the group and DM lists, and the screen
//! navigation.
//!
//! **Every control here does something.** An earlier version carried a search
//! pill, a "More unreads" pill, a "Plugins" row and a hard-coded user profile
//! ("Greg Tritthart" with an "upgrade" arrow): all four looked clickable and none
//! of them was connected to anything, so the app advertised four capabilities it
//! did not have. They are gone rather than wired, because none of them had an
//! implementation to wire — a dimmed row that does nothing is the interface
//! equivalent of a stub that returns success.
//!
//! **The list's empty state is a claim**, so it is made only when a snapshot has
//! actually delivered an answer: "No bots or groups yet" with the roster unknown
//! or failed to load would send the owner looking for a problem that is theirs
//! and is not.

use std::sync::mpsc::Sender;

use gpui::{Entity, SharedString, div, prelude::*, px, rgb};
use swarmbot_contracts::BotStatus;

use crate::proto::Command;
use crate::state::{AppState, Fetched, Screen};
use crate::theme;
use crate::widgets::bits::{self, Tone};

pub fn render(s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    div()
        .w(theme::space::SIDEBAR_W)
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::SURFACE))
        .border_r_1()
        .border_color(rgb(theme::BORDER))
        // Top search bar and new button
        .child(header(s, state))
        // Scrollable bot conversations list
        .child(
            div()
                .id("sidebar-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_2()
                .child(bots_list(s, state, commands)),
        )
        // Footer: screen navigation (the only clickable rows left in the sidebar)
        .child(footer(s, state, commands))
}

fn header(s: &AppState, state: &Entity<AppState>) -> gpui::Div {
    let state_plus = state.clone();
    let menu_open = s.plus_menu_open;
    div()
        .flex_none()
        .px_3()
        .pt_3()
        .pb_2()
        .flex()
        .items_center()
        .gap_2()
        // The app's name where the search pill used to be: the only thing in this
        // row that used to be clickable-but-dead. Jumping anywhere is `ctrl-k`,
        // which is a working feature and is named in the palette itself.
        .child(
            div()
                .flex_1()
                .h(px(36.0))
                .flex()
                .items_center()
                .text_sm()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::TEXT))
                .child("SwarmBot"),
        )
        .child(
            // Plus button opening the Add Bot / Create Group choice menu
            div()
                .id("nav-plus-menu")
                .size(px(36.0))
                .rounded_xl()
                .bg(if menu_open {
                    rgb(0xdcdce0)
                } else {
                    rgb(0xebebee)
                })
                .cursor_pointer()
                .hover(|h| h.bg(rgb(0xdcdce0)))
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(theme::TEXT))
                .text_lg()
                .font_weight(gpui::FontWeight::MEDIUM)
                .child("+")
                .on_click(move |_, _, cx: &mut gpui::App| {
                    cx.stop_propagation();
                    state_plus.update(cx, |st, cx| {
                        st.plus_menu_open = !st.plus_menu_open;
                        cx.notify();
                    });
                }),
        )
}

fn bots_list(s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    let mut list = div().flex().flex_col().gap_1();

    // 1. Group Channels Section
    let groups: Vec<&swarmbot_contracts::Conversation> = s
        .conversations
        .iter()
        .filter(|c| c.kind.is_group())
        .collect();
    if !groups.is_empty() {
        list = list.child(
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .text_xs()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::TEXT_DIM))
                .child("GROUPS"),
        );
        for group in &groups {
            let title = s.conversation_title(group);
            let conv_id = group.id.clone();
            let selected = s.open_conversation.as_ref() == Some(&conv_id);
            let working = s.is_group_working(group);
            let preview = s
                .preview(&conv_id)
                .unwrap_or_else(|| "No messages yet".to_string());
            let unread = s.unread(&conv_id);
            let state_h = state.clone();
            let conv_c = conv_id.clone();

            let state_edit = state.clone();
            let group_edit = (*group).clone();

            let state_del = state.clone();
            let conv_del = conv_id.clone();
            let title_del = title.clone();

            list = list.child(bot_item(
                &format!("group-{}", group.id),
                &title,
                theme::hex("#8b5cf6"),
                '#',
                "",
                &preview,
                unread > 0,
                selected,
                working,
                s.spinner_phase,
                move |cx| {
                    let conv = conv_c.clone();
                    state_h.update(cx, |st, cx| {
                        st.open(conv);
                        cx.notify();
                    });
                },
                Some(Box::new(move |cx: &mut gpui::App| {
                    let conv = group_edit.clone();
                    state_edit.update(cx, |st, cx| {
                        st.open_edit_group(&conv);
                        cx.notify();
                    });
                })),
                Some(Box::new(move |cx: &mut gpui::App| {
                    let conv = conv_del.clone();
                    let t = title_del.clone();
                    state_del.update(cx, |st, cx| {
                        st.confirm_delete =
                            Some(crate::state::DeleteTarget::Group { id: conv, title: t });
                        cx.notify();
                    });
                })),
            ));
        }
    }

    // 2. Direct Messages Section
    if !s.bots.is_empty() {
        list = list.child(
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .text_xs()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::TEXT_DIM))
                .child("DIRECT MESSAGES"),
        );
        for (i, bot) in s.bots.iter().enumerate() {
            let (status_text, working, _color) = status_line(&bot.status);
            let dm = bot.direct_conversation_id.clone();
            let selected = s.open_conversation.as_ref() == Some(&dm)
                || (s.open_conversation.is_none() && groups.is_empty() && i == 0);
            let state_h = state.clone();
            let state_del = state.clone();
            let dm_c = dm.clone();
            let preview = s.preview(&dm).unwrap_or(status_text);
            let unread = s.unread(&dm);
            let bot_id = bot.id.clone();
            let bot_label = bot.label.clone();

            list = list.child(bot_item(
                &format!("bot-{}", bot.id),
                &bot.label,
                theme::bot_color(bot),
                bits::initial(&bot.label),
                "",
                &preview,
                unread > 0,
                selected,
                working,
                s.spinner_phase,
                move |cx| {
                    let dm = dm_c.clone();
                    state_h.update(cx, |st, cx| {
                        st.open(dm);
                        cx.notify();
                    });
                },
                None,
                Some(Box::new(move |cx: &mut gpui::App| {
                    let bid = bot_id.clone();
                    let blab = bot_label.clone();
                    state_del.update(cx, |st, cx| {
                        st.confirm_delete = Some(crate::state::DeleteTarget::Bot {
                            id: bid,
                            label: blab,
                        });
                        cx.notify();
                    });
                })),
            ));
        }
    }

    // The empty state is a claim about the brain, so it needs the brain to have
    // answered. Unknown (still connecting, or no snapshot yet) and failed (the
    // socket reported a failure) both say what they are instead.
    let ready = matches!(s.bots, Fetched::Ready(_)) && matches!(s.conversations, Fetched::Ready(_));
    if groups.is_empty() && s.bots.is_empty() && !ready {
        list = list.child(not_ready_notice(s));
    } else if groups.is_empty() && s.bots.is_empty() {
        let state_add = state.clone();
        let commands_add = commands.clone();
        list = list.child(
            div()
                .p_4()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("No bots or groups yet"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .text_center()
                        .child("Click '+' above to add a bot or create a group."),
                )
                .child(
                    div()
                        .id("sidebar-empty-add-bot")
                        .mt_1()
                        .px_3()
                        .py_1p5()
                        .rounded_lg()
                        .bg(rgb(theme::ACCENT))
                        .hover(|h| h.bg(rgb(theme::ACCENT_HOVER)))
                        .cursor_pointer()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(0xffffff))
                        .child("+ Add Bot")
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            state_add.update(cx, |st, cx| {
                                st.open_create_bot();
                                cx.notify();
                            });
                            let _ = commands_add.send(Command::FetchBackends);
                        }),
                ),
        );
    }

    list
}

type ActionCallback = Box<dyn Fn(&mut gpui::App) + 'static>;

// ---------------------------------------------------------------------------
// Fitting a sidebar row's text into the column it actually has.
//
// **This is a character budget, and it is not a pixel measurement.** The honest
// fix for a preview line that was cut mid-glyph — the owner saw `…@waypoint, @`
// with no ellipsis — would be to hand the cell `.truncate()` and let GPUI
// ellipsize. The rows already carried `.truncate()` and still did it, and the
// reason is structural, so it is written down rather than worked around quietly:
//
// * `.truncate()` is `overflow_hidden + whitespace_nowrap + text_ellipsis`
//   (`gpui-0.2.2/src/styled.rs`).
// * `TextLayout::layout` only truncates when it can compute a width —
//   `known_dimensions.width`, else `AvailableSpace::Definite`
//   (`gpui-0.2.2/src/elements/text.rs`).
// * Taffy sizes a flex item's *content* under `AvailableSpace::MaxContent`
//   first: `determine_flex_base_size` maps definite space to max-content on the
//   main axis (`taffy-0.9.0/src/compute/flexbox.rs`). Both cells here are flex
//   items in a `flex_1().min_w_0()` chain, so their first measurement has no
//   width and the untruncated string is shaped.
// * That shape is then cached, and the cache check is
//   `wrap_width.is_none() || wrap_width == text_layout.wrap_width`. `nowrap`
//   makes `wrap_width` `None`, so the check passes on *every* later pass and the
//   full-width size is reused: the cell gets no ellipsis and the row clips it.
//
// So the string is shortened in Rust to the column's budget, which does put a
// glyph we choose at the end instead of letting the row edge pick one. It is
// approximate by construction — a proportional font makes exact columns
// impossible without measuring — and it is deliberately *short*, so the
// ellipsis lands inside the column instead of being the thing that gets clipped.
// `.truncate()` stays on both cells as the second line of defence.

/// The preview line's column, in pixels: `SIDEBAR_W` 280 − scroll `px_2` (16) −
/// row `px_3` (24) − [`theme::space::AVATAR`] (32) − row `gap_3` (12) = 196, less
/// the trailing `gap_1` (4), unread dot (7), `gap_1` (4) and spinner (13) that
/// the preview shares its row with = 168. Rounded up to 170 and then spent on
/// wide glyph advances, so the budget lands short rather than long.
const PREVIEW_W: f32 = 170.0;
/// The name's column: the same 196 less the row's trailing `gap_2` (8) and the
/// edit + delete affordances (`px_1` around a 12px glyph each, plus their
/// `gap_1p5`) = 146.
const NAME_W: f32 = 146.0;
/// A deliberately wide average advance for the UI sans at 12px and at 14px bold —
/// the real average is nearer 6.0 and 7.2. Erring wide makes the budget err
/// short, which is the direction that leaves the `…` visible instead of clipping
/// it.
const PREVIEW_CHAR_W: f32 = 6.6;
const NAME_CHAR_W: f32 = 7.8;

/// How many characters of a preview line the sidebar shows before `…`.
const PREVIEW_CHARS: usize = char_budget(PREVIEW_W, PREVIEW_CHAR_W);
/// How many characters of a bot/group name the sidebar shows before `…`.
const NAME_CHARS: usize = char_budget(NAME_W, NAME_CHAR_W);

/// How many characters of the given average advance fit in `width` pixels,
/// floored and never below one.
///
/// A `const fn` may do float arithmetic on this toolchain; the cast truncates
/// toward zero, which for two positive literals is the floor a budget wants. The
/// constants above are literals, so no value here depends on runtime data.
/// `pub(crate)` so the palette sizes its own columns with the same rule instead
/// of a second copy that could drift.
pub(crate) const fn char_budget(width: f32, advance: f32) -> usize {
    let cells = (width / advance) as usize;
    if cells == 0 { 1 } else { cells }
}

/// Shorten `text` to at most `max_chars` characters, ending in `…` when anything
/// was dropped.
///
/// See the block comment above for why this exists in Rust instead of being left
/// to `.truncate()`; why it is character-based rather than measured; and why the
/// budget is conservative. `pub(crate)` because the command palette's rows are
/// the same shape — a `truncate()` in a flex row with no definite width — and a
/// second copy of this would be a second set of rules to keep in step.
pub(crate) fn ellipsize(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    // One column of the budget is spent on the ellipsis itself, so the result
    // always fits the width the budget was derived from.
    let mut out: String = text.chars().take(max_chars - 1).collect();
    // A trailing space before `…` reads as a typo rather than as a truncation.
    while out.ends_with([' ', '\t']) {
        out.pop();
    }
    out.push('…');
    out
}

#[allow(clippy::too_many_arguments)]
fn bot_item(
    id: &str,
    label: &str,
    color: gpui::Rgba,
    initial: char,
    time: &str,
    preview: &str,
    unread: bool,
    selected: bool,
    working: bool,
    spinner_phase: usize,
    on_click: impl Fn(&mut gpui::App) + 'static,
    on_edit: Option<ActionCallback>,
    on_delete: Option<ActionCallback>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id.to_string()))
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .rounded_2xl()
        .cursor_pointer()
        .when(selected, |d| d.bg(rgb(theme::SURFACE_RAISED)))
        .hover(|h| h.bg(rgb(0xebebee)))
        // The documented avatar metric, not a bare number: this row used to draw
        // the same bot identity at 38px while `theme::space::AVATAR` says 32 and
        // the transcript draws it at 28 — three sizes for one thing. The sidebar
        // takes the documented size rather than a fourth value; a locally named
        // const would only be an excuse to drift again.
        .child(bits::avatar(color, initial, theme::space::AVATAR))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_col()
                .gap_0p5()
                // Row 1: Name + Time + Delete button
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
                                .text_sm()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(theme::TEXT))
                                // `.truncate()` alone cut the name at the row
                                // edge; see the block comment above `ellipsize`.
                                .truncate()
                                .child(ellipsize(label, NAME_CHARS)),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .when(!time.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_DIM))
                                            .child(time.to_string()),
                                    )
                                })
                                .when_some(on_edit, |d, on_ed| {
                                    d.child(
                                        div()
                                            .id(SharedString::from(format!("edit-{}", id)))
                                            .cursor_pointer()
                                            .px_1()
                                            .rounded_md()
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_DIM))
                                            .hover(|h| {
                                                h.text_color(rgb(theme::ACCENT))
                                                    .bg(rgb(theme::SURFACE_RAISED))
                                            })
                                            .child("✎")
                                            .on_click(move |_, _, cx: &mut gpui::App| {
                                                cx.stop_propagation();
                                                on_ed(cx);
                                            }),
                                    )
                                })
                                .when_some(on_delete, |d, on_del| {
                                    d.child(
                                        div()
                                            .id(SharedString::from(format!("del-{}", id)))
                                            .cursor_pointer()
                                            .px_1()
                                            .rounded_md()
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_DIM))
                                            .hover(|h| {
                                                // `DANGER` is a *fill* (3.5:1 on
                                                // white); this glyph is text, so
                                                // it takes the readable twin.
                                                h.text_color(rgb(theme::DANGER_TEXT))
                                                    .bg(rgb(0xfee2e2))
                                            })
                                            .child("✕")
                                            .on_click(move |_, _, cx: &mut gpui::App| {
                                                cx.stop_propagation();
                                                on_del(cx);
                                            }),
                                    )
                                }),
                        ),
                )
                // Row 2: Preview + Unread dot
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_MUTED))
                                // Measured on 2026-11-15: the row showed
                                // "Group created with @waypoint, @" — cut at the
                                // trailing-icon boundary with no `…`, because
                                // `.truncate()` never reached a definite width
                                // through the `flex_1().min_w_0()` chain.
                                .truncate()
                                .child(ellipsize(preview, PREVIEW_CHARS)),
                        )
                        .when(unread, |d| {
                            d.child(
                                div()
                                    .size(px(7.0))
                                    .flex_none()
                                    .rounded_full()
                                    // `theme::ACCENT`, not a copy of it: this
                                    // literal was the pre-2026-11-15 accent
                                    // (`#2563eb`), left behind when the token
                                    // moved to `#1d4ed8`.
                                    .bg(rgb(theme::ACCENT)),
                            )
                        })
                        .when(working, |d| d.child(bits::spinner(spinner_phase))),
                ),
        )
        .on_click(move |_, _, cx: &mut gpui::App| {
            on_click(cx);
        })
}

/// How a footer nav button is drawn.
///
/// Split out as a value rather than left inline because it is the whole of the
/// defect: every button used to get the same ghost treatment, so the sidebar
/// could not answer "which screen am I on?". A screen that is not a nav screen
/// (`Screen::Conversation`, the default) leaves all five idle, which is correct
/// — the footer marks one of its own buttons, it does not invent a sixth state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NavLook {
    /// The screen this button opens is not the open one.
    Idle,
    /// The screen this button opens is the open one.
    Active,
}

/// The look of the nav button that opens `button_screen` while `open` is the
/// open screen.
pub(crate) fn nav_look(button_screen: Screen, open: Screen) -> NavLook {
    if button_screen == open {
        NavLook::Active
    } else {
        NavLook::Idle
    }
}

/// One footer nav button: a ghost button, filled with the accent when its screen
/// is the open one.
///
/// The boundary comes from `bits::button`'s own border (a ghost fill of
/// `SURFACE_RAISED` on the sidebar's `SURFACE` is 1.09:1, so the edge has to be
/// drawn); the *active* fill is what this adds.
fn nav_button(
    label: &str,
    screen: Screen,
    open: Screen,
    id: &'static str,
) -> gpui::Stateful<gpui::Div> {
    bits::button(label, Tone::Ghost)
        .when(nav_look(screen, open) == NavLook::Active, |d| {
            d.bg(rgb(theme::ACCENT))
                .border_color(rgb(theme::ACCENT))
                .text_color(rgb(0xffffff))
                .font_weight(gpui::FontWeight::MEDIUM)
        })
        .id(id)
}

fn footer(s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    let state_settings = state.clone();
    let state_nodes = state.clone();
    let state_routines = state.clone();
    let state_usage = state.clone();
    let state_rules = state.clone();
    let commands_settings = commands.clone();
    let commands_nodes = commands.clone();
    let commands_routines = commands.clone();
    let commands_usage = commands.clone();
    let commands_rules = commands.clone();
    // The window the usage screen last asked for, read here so the nav button
    // asks the same question the screen is showing.
    let usage_hours = s.usage_hours;
    // Read once: every button asks the same question of the same snapshot.
    let open = s.screen;

    div()
        .flex_none()
        .border_t_1()
        .border_color(rgb(theme::BORDER))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .child(
                    nav_button("Rules", Screen::Rules, open, "nav-rules").on_click(
                        move |_, _, cx: &mut gpui::App| {
                            state_rules.update(cx, |st, cx| {
                                st.screen = Screen::Rules;
                                cx.notify();
                            });
                            let _ = commands_rules.send(Command::FetchRules);
                        },
                    ),
                )
                .child(
                    nav_button("Nodes", Screen::Nodes, open, "nav-nodes").on_click(
                        move |_, _, cx: &mut gpui::App| {
                            state_nodes.update(cx, |st, cx| {
                                st.screen = Screen::Nodes;
                                cx.notify();
                            });
                            let _ = commands_nodes.send(Command::FetchNodes);
                        },
                    ),
                )
                .child(
                    nav_button("Routines", Screen::Routines, open, "nav-routines").on_click(
                        move |_, _, cx: &mut gpui::App| {
                            state_routines.update(cx, |st, cx| {
                                st.screen = Screen::Routines;
                                cx.notify();
                            });
                            let _ = commands_routines.send(Command::FetchRoutines);
                        },
                    ),
                )
                .child(
                    nav_button("Settings", Screen::Settings, open, "nav-settings").on_click(
                        move |_, _, cx: &mut gpui::App| {
                            state_settings.update(cx, |st, cx| {
                                st.screen = Screen::Settings;
                                cx.notify();
                            });
                            let _ = commands_settings.send(Command::FetchSettings);
                            let _ = commands_settings.send(Command::FetchBackends);
                        },
                    ),
                )
                .child(
                    nav_button("Usage", Screen::Usage, open, "nav-usage").on_click(
                        move |_, _, cx: &mut gpui::App| {
                            state_usage.update(cx, |st, cx| {
                                st.screen = Screen::Usage;
                                cx.notify();
                            });
                            let _ = commands_usage.send(Command::FetchUsage {
                                since: Some(crate::view::usage::since_for(
                                    usage_hours,
                                    chrono::Utc::now(),
                                )),
                            });
                        },
                    ),
                ),
        )
}

/// What the list says while the roster is not known.
///
/// The roster has exactly one source — the live snapshot — so it cannot fail on
/// its own: a failure *is* the socket's failure, and [`AppState::conn`] already
/// carries it. That is why this reads the connection label instead of inventing
/// a second error state that nothing could ever set.
///
/// Deliberately no action button: the only thing that helps is waiting for the
/// socket, and the `+` menu above already works.
fn not_ready_notice(s: &AppState) -> gpui::Div {
    div()
        .p_4()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(theme::TEXT_MUTED))
                .text_center()
                .child(match s.conn {
                    crate::state::ConnectionState::Connected => "Waiting for the first snapshot",
                    _ => "Not connected",
                }),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .text_center()
                .child(format!(
                    "{}. The roster is shown once the live socket delivers its snapshot.",
                    s.conn.label()
                )),
        )
}

/// The preview line's text and whether the bot is working, plus the colour the
/// caller should draw that text in.
///
/// The third element is a **text** colour, so it comes from the `*_TEXT` tokens
/// and never from the vivid `DANGER`/`WARNING`/`SUCCESS` fills: those are 3.5:1,
/// 2.1:1 and 2.4:1 on white, which is fine for a 9px dot and not enough for a
/// 12px word. The caller discards it today; keeping the table honest means the
/// next caller that uses it cannot reintroduce the unreadable pair.
fn status_line(status: &BotStatus) -> (String, bool, u32) {
    match status {
        BotStatus::Idle => ("idle".to_string(), false, theme::TEXT_DIM),
        BotStatus::Working { activity, .. } => (activity.clone(), true, theme::ACCENT_HOVER),
        BotStatus::Queued { count } => (format!("{count} queued"), false, theme::WARNING_TEXT),
        BotStatus::Error { reason } => (reason.clone(), false, theme::DANGER_TEXT),
        BotStatus::Disabled => ("disabled".to_string(), false, theme::TEXT_DIM),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Defect: a 60-character preview in a column that fits ~27 characters was
    // cut at the row edge with no ellipsis ("…@waypoint, @" on the owner's
    // screenshot). `ellipsize` is the budget; these tests pin the three things
    // that made the old rendering wrong.
    // -----------------------------------------------------------------------

    /// Long text ends in an ellipsis and never exceeds the budget, so the `…` is
    /// inside the column instead of being the thing that gets clipped.
    #[test]
    fn a_long_preview_ends_in_an_ellipsis_within_the_budget() {
        let preview = "Group created with @waypoint, @alfred, @scout and @sentinel";
        let fitted = ellipsize(preview, 24);
        assert!(
            fitted.ends_with('…'),
            "the old code handed the full 57-char string to a ~196px cell and the row edge cut it: {fitted:?}"
        );
        assert_eq!(
            fitted.chars().count(),
            24,
            "the result spends one column on the ellipsis and fills the rest: {fitted:?}"
        );
        assert!(
            preview.starts_with(fitted.trim_end_matches('…')),
            "the text is shortened from the end, never altered in the middle: {fitted:?}"
        );
    }

    /// Text that already fits is returned unchanged — no ellipsis, no rewrite.
    #[test]
    fn text_that_fits_is_untouched() {
        assert_eq!(ellipsize("short", 24), "short");
        // Exactly at the budget is still untouched: the truncation is a result of
        // the text being too long, not of the budget being reached.
        assert_eq!(
            ellipsize("123456789012345678901234", 24),
            "123456789012345678901234"
        );
    }

    /// The budget is counted in characters, so a multi-byte glyph is one column:
    /// `@wánder` must not lose its tail because `é` is two bytes.
    #[test]
    fn the_budget_counts_characters_not_bytes() {
        let fitted = ellipsize("ÿÿÿÿ", 3);
        assert_eq!(fitted, "ÿÿ…", "a 2-byte char is one column: {fitted:?}");
        assert_eq!(fitted.chars().count(), 3);
    }

    /// A space immediately before the cut is dropped, because `"… @waypoint …"`
    /// with a dangling space reads as a typo rather than as a truncation.
    #[test]
    fn a_trailing_space_is_not_kept_before_the_ellipsis() {
        assert_eq!(ellipsize("Group created with", 8), "Group c…");
        let trimmed = ellipsize("Group created with", 7);
        assert_eq!(
            trimmed, "Group…",
            "the space goes, not a letter: {trimmed:?}"
        );
        assert!(
            trimmed.chars().count() <= 7,
            "dropping the space must keep the result inside its budget, not push the ellipsis out of it: {trimmed:?}"
        );
    }

    /// A zero budget is empty rather than a panic or an ellipsis with no room.
    #[test]
    fn a_zero_budget_cannot_panic() {
        assert_eq!(ellipsize("anything", 0), "");
        assert_eq!(ellipsize("", 0), "");
        assert_eq!(ellipsize("", 10), "");
    }

    /// The name budget is tighter than the preview budget and both are wide
    /// enough to be useful: a row shows less of its name than of its preview,
    /// because the name's row also carries the edit and delete affordances.
    #[test]
    fn the_two_budgets_are_ordered_and_sane() {
        // Const blocks, not runtime assertions: these are facts about literals,
        // and clippy is right that a runtime `assert!` on constants is noise.
        const {
            assert!(
                NAME_CHARS < PREVIEW_CHARS,
                "the name column is narrower than the preview column: its row also \
                 carries the edit and delete affordances"
            );
            assert!(
                PREVIEW_CHARS >= 16 && PREVIEW_CHARS <= 32,
                "a 12px preview in a ~170px column fits roughly two dozen characters"
            );
            // `AppState::preview` already caps at 60 chars, which is far wider
            // than this column: the UI must shorten further, or the `…` is the
            // character that gets clipped.
            assert!(
                PREVIEW_CHARS < 60,
                "the sidebar budget must be narrower than the state's 60-char cap"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Defect: the footer's five nav buttons all looked identical, so the open
    // screen was indistinguishable from the other four.
    // -----------------------------------------------------------------------

    /// Every nav screen marks exactly its own button, and nothing else.
    #[test]
    fn each_nav_screen_marks_exactly_its_own_button() {
        let buttons = [
            Screen::Rules,
            Screen::Nodes,
            Screen::Routines,
            Screen::Settings,
            Screen::Usage,
        ];
        for open in buttons {
            let active: Vec<Screen> = buttons
                .iter()
                .copied()
                .filter(|button| nav_look(*button, open) == NavLook::Active)
                .collect();
            assert_eq!(
                active,
                vec![open],
                "with {open:?} open the footer must mark exactly that button; \
                 before this every button was idle and the sidebar could not answer \
                 \"where am I?\""
            );
        }
    }

    /// WCAG 2.x contrast ratio for two `0xrrggbb` colours.
    fn contrast(a: u32, b: u32) -> f64 {
        let channel = |shift: u32, color: u32| {
            let v = ((color >> shift) & 0xff) as f64 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance = |color: u32| {
            0.2126 * channel(16, color) + 0.7152 * channel(8, color) + 0.0722 * channel(0, color)
        };
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// The active pill is white on the accent, and both halves of that choice are
    /// load-bearing: white on `ACCENT` has to clear the 4.5:1 body-text bar, and
    /// the accent fill has to be distinguishable from the sidebar surface the
    /// idle ghosts sit on — otherwise "which screen is open" is answered with a
    /// colour nobody can see, which is the defect again in a different hue.
    #[test]
    fn the_active_nav_pill_is_readable_and_stands_out_from_the_sidebar() {
        const WHITE: u32 = 0xffffff;
        let label = contrast(WHITE, theme::ACCENT);
        assert!(
            label >= 4.5,
            "the active pill's label is {label:.2}:1 on the accent fill"
        );
        let fill_vs_surface = contrast(theme::ACCENT, theme::SURFACE);
        assert!(
            fill_vs_surface >= 3.0,
            "the active pill's fill is only {fill_vs_surface:.2}:1 against the sidebar, \
             so the marked screen is not marked"
        );
    }

    /// The conversation pane is not one of the footer's screens, so none of them
    /// is marked while it is open — the footer does not invent a sixth state.
    #[test]
    fn the_conversation_screen_leaves_every_nav_button_idle() {
        for button in [
            Screen::Rules,
            Screen::Nodes,
            Screen::Routines,
            Screen::Settings,
            Screen::Usage,
        ] {
            assert_eq!(nav_look(button, Screen::Conversation), NavLook::Idle);
        }
    }

    /// The status line draws text, so its colour is a `*_TEXT` token and never a
    /// vivid fill — `WARNING` is 2.1:1 and `DANGER` 3.5:1 on white.
    #[test]
    fn the_status_line_uses_readable_text_tokens_not_fills() {
        for status in [
            BotStatus::Idle,
            BotStatus::Working {
                conversation_id: swarmbot_contracts::ConversationId::new(),
                activity: "reading".into(),
            },
            BotStatus::Queued { count: 2 },
            BotStatus::Error {
                reason: "boom".into(),
            },
            BotStatus::Disabled,
        ] {
            let (_, _, color) = status_line(&status);
            assert!(
                color != theme::DANGER && color != theme::SUCCESS && color != theme::WARNING,
                "{status:?} is drawn as text but got the vivid fill {color:#08x}"
            );
        }
    }
}

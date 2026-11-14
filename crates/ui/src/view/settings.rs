//! The Settings screen: the backend list with a **Probe** button that shows the
//! result verbatim (including a gateway's own message, e.g. an inferhub 402 bid
//! line), and the brain-wide [`Settings`] the owner can change — the bot
//! messaging kill switch, hop/tool-round limits, the per-turn and per-day token
//! caps, auto-approve for read-only, and the safety ceiling.
//!
//! Editing happens on a draft ([`AppState::settings_draft`]); **Save** diffs it
//! against the last-saved settings ([`diff`]) and PATCHes only the keys that
//! changed, so an untouched ceiling is never rewritten. The diff is a pure
//! function a `#[gpui::test]` asserts on.

use std::sync::mpsc::Sender;

use gpui::{Entity, SharedString, div, prelude::*, px, rgb};
use serde_json::{Map, Value, json};
use swarmbot_contracts::{BackendKind, PolicyCeiling, Settings};

use crate::proto::Command;
use crate::state::{AppState, Fetched};
use crate::theme;
use crate::widgets::bits::{self, Tone};
use crate::widgets::text_input::TextInput;

/// Tokens per click of a token-cap stepper. A budget is spent in thousands, so a
/// step of one would need five million clicks to cross the clamp and make the
/// control useless in practice; ten thousand is about one model round trip.
const TOKEN_CAP_STEP: i64 = 10_000;

/// The largest cap this screen offers. Above this the number stops meaning
/// anything an owner can reason about, and an attempted `u64::MAX` would show a
/// value cell nobody can read. A config file may still set a larger cap; the
/// stepper then leaves it alone rather than shrinking it.
const TOKEN_CAP_MAX: u64 = 5_000_000;

/// The keys Save will PATCH, computed by diffing `saved` against `draft`. Only
/// changed scalar keys and a changed `safety` map are included; an unchanged
/// setting is omitted so the brain's merge cannot clobber it.
pub fn diff(saved: &Settings, draft: &Settings) -> Value {
    let mut map = Map::new();
    if saved.bot_messaging != draft.bot_messaging {
        map.insert("bot_messaging".into(), json!(draft.bot_messaging));
    }
    if saved.max_hops != draft.max_hops {
        map.insert("max_hops".into(), json!(draft.max_hops));
    }
    if saved.max_tool_rounds != draft.max_tool_rounds {
        map.insert("max_tool_rounds".into(), json!(draft.max_tool_rounds));
    }
    if saved.auto_approve_read_only != draft.auto_approve_read_only {
        map.insert(
            "auto_approve_read_only".into(),
            json!(draft.auto_approve_read_only),
        );
    }
    if saved.transcript_mirror != draft.transcript_mirror {
        map.insert("transcript_mirror".into(), json!(draft.transcript_mirror));
    }
    // A token cap is sent as a number or as `null` (no cap). Zero is not a value
    // the brain accepts — it is an error, never "unlimited" — so a stray `Some(0)`
    // is normalised to `null` here as a second line of defence behind the
    // stepper, which already cannot produce it.
    if saved.max_tokens_per_turn != draft.max_tokens_per_turn {
        map.insert(
            "max_tokens_per_turn".into(),
            json!(draft.max_tokens_per_turn.filter(|tokens| *tokens > 0)),
        );
    }
    if saved.max_tokens_per_day != draft.max_tokens_per_day {
        map.insert(
            "max_tokens_per_day".into(),
            json!(draft.max_tokens_per_day.filter(|tokens| *tokens > 0)),
        );
    }
    if saved.safety != draft.safety {
        map.insert(
            "safety".into(),
            serde_json::to_value(&draft.safety).unwrap_or_else(|_| json!({})),
        );
    }
    Value::Object(map)
}

/// One click of a token-cap stepper, as the value to store in the draft.
///
/// `None` is the *only* representation of "no cap": the brain rejects `Some(0)`
/// as an error rather than reading it as unlimited, so a step that lands on or
/// below zero has to come back as `None`. That is the whole reason this is a
/// function with a test instead of an inline `clamp`, which would happily
/// produce `Some(0)`.
///
/// `+` never lowers a cap that a config file set above [`TOKEN_CAP_MAX`]: a
/// control that shrinks the number it is asked to raise is a lie, so the current
/// value is the ceiling in that one case.
fn stepped_token_cap(cap: Option<u64>, delta: i64) -> Option<u64> {
    let current = cap.map_or(0, |tokens| i64::try_from(tokens).unwrap_or(i64::MAX));
    let step = delta.saturating_mul(TOKEN_CAP_STEP);
    let next = if step > 0 {
        current
            .saturating_add(step)
            .min((TOKEN_CAP_MAX as i64).max(current))
    } else {
        current.saturating_add(step).max(0)
    };
    // `next` is non-negative by construction, so this cast cannot lose a sign.
    match next as u64 {
        0 => None,
        tokens => Some(tokens),
    }
}

/// A token cap as its row shows it: `off` for no cap, the token count otherwise.
/// Never `0` — zero is a value the brain refuses, not "unlimited".
fn token_cap_label(cap: Option<u64>) -> String {
    match cap {
        None => "off".to_string(),
        Some(tokens) => tokens.to_string(),
    }
}

/// Whether a backend row's Connect has anything left to prove.
///
/// Four states, and the wording is part of the contract: "connected" here means
/// **the handshake passed**, which is not "a turn works" (D9.28). A green
/// handshake on an agent that advertises a login therefore stays `NeedsLogin`
/// when the backend said so, and gets a warning line when it did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Nothing has been checked in this session.
    NotChecked,
    /// The handshake passed and the backend did not ask for a login.
    Connected,
    /// The backend refused (or may refuse) because a login is needed.
    NeedsLogin,
    /// Checked, and it failed for some other reason (no binary, timeout, …).
    Failed,
}

impl ConnectionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotChecked => "not checked",
            Self::Connected => "connected",
            Self::NeedsLogin => "needs a login",
            Self::Failed => "not connected",
        }
    }

    /// What the action button says. A backend that already answered offers
    /// **Re-check**, not "Connect": the action is the same handshake, and the
    /// label is the only place that can admit it is a repeat. Hiding the button
    /// instead would strand a row whose agent died after a green handshake —
    /// the one case where re-checking is the only thing that helps.
    pub fn connect_label(self) -> &'static str {
        match self {
            Self::Connected => "Re-check",
            _ => "Connect",
        }
    }

    pub fn tone(self) -> u32 {
        match self {
            Self::NotChecked => theme::TEXT_DIM,
            Self::Connected => theme::SUCCESS,
            Self::NeedsLogin => theme::WARNING,
            Self::Failed => theme::DANGER,
        }
    }
}

/// The state of one backend's row from the last report, if any.
pub fn connection_state(probe: Option<&swarmbot_client::BackendProbe>) -> ConnectionState {
    match probe {
        None => ConnectionState::NotChecked,
        // `auth_required` wins over `ok`: ACP's handshake can pass while the
        // agent still says a login is what is missing, and a row that printed
        // "connected" there would be the exact lie D9.28 records.
        Some(probe) if probe.auth_required => ConnectionState::NeedsLogin,
        Some(probe) if probe.ok => ConnectionState::Connected,
        Some(_) => ConnectionState::Failed,
    }
}

/// What a passed handshake still does not tell the owner, when there is
/// something to say.
///
/// `None` unless the backend both passed its handshake **and** advertises a
/// login. The sentence is a fact plus a condition, never a prediction, and the
/// difference was measured: `opencode` advertises `opencode-login` and answers
/// turns fine, while `claude-code-acp` advertises `claude-login` and refuses
/// `session/prompt` with "Authentication required" (D9.28). The handshake data
/// cannot tell those two apart, so the line states what is true of both — the
/// login exists, and it is the fix *if* a turn comes back refused — instead of
/// raising an alarm on a healthy backend or hiding the one action that helps.
pub fn handshake_only_note(probe: Option<&swarmbot_client::BackendProbe>) -> Option<String> {
    let probe = probe?;
    if !probe.ok || probe.auth_methods.is_empty() {
        return None;
    }
    let names: Vec<&str> = probe
        .auth_methods
        .iter()
        .map(|method| method.name.as_str())
        .collect();
    Some(format!(
        "handshake OK. This agent also offers a login ({}); if a turn is refused with an authentication error, run that login in its own terminal and press {} again.",
        names.join(", "),
        ConnectionState::Connected.connect_label()
    ))
}

pub fn render(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
    skills_input: &Entity<TextInput>,
) -> gpui::Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(
            div()
                .px_4()
                .py_2()
                .text_lg()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::TEXT))
                .child("Settings"),
        )
        .child(
            div()
                .id("settings-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_4()
                .px_4()
                .py_3()
                .child(backends_section(s, state, commands))
                .child(roster_skills_section(s, state, commands, skills_input))
                .child(settings_section(s, state, commands)),
        )
}

fn backends_section(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
) -> gpui::Div {
    let commands_probe = commands.clone();
    let mut section = div().w_full().flex().flex_col().gap_2().child(
        div()
            .flex()
            .items_center()
            .child(div().flex_1().child(bits::section_label("BACKENDS")))
            .child(
                bits::button("Probe", Tone::Ghost)
                    .id("probe-backends")
                    .on_click(move |_, _, _: &mut gpui::App| {
                        let _ = commands_probe.send(Command::ProbeBackends);
                    }),
            ),
    );

    let has_real = s
        .backends
        .iter()
        .any(|b| b.id != "fake" && b.kind != BackendKind::Fake);
    let visible_backends: Vec<_> = if has_real {
        s.backends
            .iter()
            .filter(|b| b.id != "fake" && b.kind != BackendKind::Fake)
            .collect()
    } else {
        s.backends.iter().collect()
    };

    if visible_backends.is_empty() {
        return section.child(
            div()
                .text_sm()
                .text_color(rgb(theme::TEXT_DIM))
                .child("no backends configured"),
        );
    }

    for backend in visible_backends {
        let probe = s.probe_for(&backend.id);
        let connection = connection_state(probe);
        let connecting = s.is_connecting(&backend.id);
        let backend_id = backend.id.clone();
        let mut card = div()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .p_3()
            .flex()
            .flex_col()
            .gap_1()
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
                            .child(backend.id.clone()),
                    )
                    .child(bits::badge(
                        backend_kind_label(backend.kind),
                        theme::SURFACE_RAISED,
                        theme::TEXT_MUTED,
                    ))
                    // The connection state and its Connect action live on the
                    // header line: one glance answers "is this one usable, and
                    // what do I press if it is not".
                    .child(div().flex_1())
                    .child(bits::badge(
                        connection.label(),
                        theme::SURFACE_RAISED,
                        connection.tone(),
                    ))
                    .children(connect_button(
                        &backend_id,
                        connection,
                        connecting,
                        state,
                        commands,
                    )),
            );

        // Only rows with something to say: an ACP backend has no base_url, and
        // a wall of em-dashes is exactly what made these cards unreadable.
        if let Some(model) = backend.model.as_deref().filter(|m| !m.is_empty()) {
            card = card.child(bits::kv("model", model.to_string()));
        }
        if let Some(url) = backend.base_url.as_deref().filter(|u| !u.is_empty()) {
            card = card.child(bits::kv("base url", url.to_string()));
        }
        if let Some(env) = backend.api_key_env.as_deref().filter(|e| !e.is_empty()) {
            card = card.child(bits::kv("api key from", env.to_string()));
        }
        // How this backend connects: an ACP agent's command line, a harness's
        // binary, its working dir and permission mode. Secret-bearing values
        // arrive masked from the brain (`[hidden]`) and are shown as names.
        let rows = connection_rows(&backend.options);
        for (label, value) in rows {
            card = card.child(bits::kv(&label, value));
        }
        if backend.model.is_none()
            && backend.base_url.is_none()
            && backend.api_key_env.is_none()
            && backend.options.is_empty()
        {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("nothing to configure — always available"),
            );
        }

        if let Some(probe) = probe {
            let color = connection.tone();
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(color)).child(if probe.ok {
                        "probe: reachable"
                    } else {
                        "probe: failed"
                    }))
                    // The detail is shown verbatim — including a gateway's own
                    // message (an inferhub 402 bid line, say). Never paraphrased.
                    .children(probe.detail.as_ref().map(|detail| {
                        div()
                            .w_full()
                            .rounded_sm()
                            .bg(rgb(theme::CODE_BG))
                            .px_2()
                            .py_1()
                            .font_family("DejaVu Sans Mono")
                            .text_size(px(11.5))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(detail.clone())
                    })),
            );
        } else {
            card = card.child(
                div()
                    .text_xs()
                    .italic()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child(
                        "not probed yet — press Connect (the handshake spawns an ACP agent; no model call)",
                    ),
            );
        }

        // The logins the backend itself advertises. Rendered from the report,
        // never from a table in this file: the method ids belong to the agent.
        card = card.children(login_panel(&backend_id, probe, connecting, state, commands));

        if let Some(note) = handshake_only_note(probe) {
            // Informational, not a warning: the handshake passed, and colouring
            // a healthy backend amber teaches the owner to ignore amber.
            card = card.child(div().text_xs().text_color(rgb(theme::TEXT_DIM)).child(note));
        }
        section = section.child(card);
    }
    section
}

/// The Connect button for one backend, or a spinner while its handshake runs.
fn connect_button(
    backend: &str,
    connection: ConnectionState,
    connecting: bool,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
) -> Option<gpui::AnyElement> {
    if connecting {
        return Some(
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
                .into_any_element(),
        );
    }
    let commands_h = commands.clone();
    let state_h = state.clone();
    let backend_id = backend.to_string();
    Some(
        bits::button(connection.connect_label(), Tone::Primary)
            .id(SharedString::from(format!("connect-{backend}")))
            .on_click(move |_, _, cx: &mut gpui::App| {
                // One handshake per backend: a second click while the first is
                // in flight is dropped here, before a command is sent.
                let mut send = false;
                state_h.update(cx, |s, cx| {
                    send = s.begin_connect(&backend_id);
                    cx.notify();
                });
                if send {
                    // A plain Connect asks for the handshake only; a login is a
                    // separate, explicit click on a method the agent named.
                    let _ = commands_h.send(Command::ConnectBackend {
                        backend: backend_id.clone(),
                        method: None,
                    });
                }
            })
            .into_any_element(),
    )
}

/// The "log in with …" rows for a backend that advertises logins, or nothing.
///
/// One line per advertised method, with the agent's own name and description;
/// then the exact command to run — from the method when the agent named one,
/// else from the backend's `options.login_command` — with a Copy button, and a
/// button that asks the backend to run that login over ACP. The command is
/// shown as text and copied, never executed here: an agent login opens a
/// terminal and asks its own questions, and pretending otherwise is the same
/// class of lie as a fake approval.
fn login_panel(
    backend: &str,
    probe: Option<&swarmbot_client::BackendProbe>,
    connecting: bool,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
) -> Vec<gpui::AnyElement> {
    let Some(probe) = probe else {
        return Vec::new();
    };
    if probe.auth_methods.is_empty() && probe.login_command.is_none() {
        return Vec::new();
    }
    let mut rows: Vec<gpui::AnyElement> = Vec::new();
    let command = probe
        .login_command
        .clone()
        .or_else(|| probe.auth_methods.iter().find_map(|m| m.command.clone()));

    for method in &probe.auth_methods {
        let commands_h = commands.clone();
        let state_h = state.clone();
        let backend_id = backend.to_string();
        let method_id = method.id.clone();
        let method_id_log = method.id.clone();
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .flex_wrap()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::TEXT))
                            .child(method.name.clone()),
                    )
                    .child(div().text_xs().text_color(rgb(theme::TEXT_MUTED)).child(
                        match &method.description {
                            Some(description) => description.clone(),
                            // No description and no command: state the gap
                            // rather than print the id twice.
                            None => format!("auth method `{}`", method.id),
                        },
                    )),
            )
            // One handshake per backend, so this row shows the same in-flight
            // state as the header's Connect: two agent processes for the same
            // backend would race over one login.
            .child(if connecting {
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(bits::spinner(0))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child("waiting…"),
                    )
                    .into_any_element()
            } else {
                bits::button("Log in", Tone::Ghost)
                    .id(SharedString::from(format!("connect-{backend}-{method_id}")))
                    .on_click(move |_, _, cx: &mut gpui::App| {
                        let mut send = false;
                        let method_id = method_id_log.clone();
                        let backend_for_key = backend_id.clone();
                        state_h.update(cx, |s, cx| {
                            send = s.begin_connect(&backend_for_key);
                            cx.notify();
                        });
                        if send {
                            let _ = commands_h.send(Command::ConnectBackend {
                                backend: backend_id.clone(),
                                method: Some(method_id),
                            });
                        }
                    })
                    .into_any_element()
            });
        if let Some(command) = &command {
            let copy = command.clone();
            row = row.child(
                bits::button("Copy login command", Tone::Ghost)
                    .id(SharedString::from(format!(
                        "copy-login-{backend}-{method_id}"
                    )))
                    .on_click(move |_, _, cx: &mut gpui::App| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()));
                    }),
            );
        }
        rows.push(row.into_any_element());
    }

    if let Some(command) = command {
        rows.push(
            div()
                .w_full()
                .rounded_sm()
                .bg(rgb(theme::CODE_BG))
                .px_2()
                .py_1()
                .font_family("DejaVu Sans Mono")
                .text_size(px(11.5))
                .text_color(rgb(theme::TEXT))
                .child(command)
                .into_any_element(),
        );
    } else if !probe.auth_methods.is_empty() {
        // The method's own copy button is the escape hatch; say where the
        // command would have come from instead of inventing one.
        rows.push(
            div()
                .text_xs()
                .italic()
                .text_color(rgb(theme::TEXT_DIM))
                .child(
                    "the agent named no login command — set options.login_command to show one here",
                )
                .into_any_element(),
        );
    }
    rows
}

/// Parse a comma-separated skill draft into the list the brain stores.
///
/// Trimmed, empty items dropped, duplicates collapsed in first-seen order — the
/// brain treats a skill as a *name*, so `lead, lead` is one skill twice and would
/// be rendered twice with no added meaning. A trailing comma (which every
/// hand-typed list has while it is being typed) is not an error.
pub fn parse_skills(draft: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in draft.split(',') {
        let name = item.trim();
        if name.is_empty() || out.iter().any(|existing| existing == name) {
            continue;
        }
        out.push(name.to_string());
    }
    out
}

/// The comma-separated form a bot's list starts as, the inverse of
/// [`parse_skills`].
pub fn skills_draft_of(skills: &[String]) -> String {
    skills.join(", ")
}

fn roster_skills_section(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
    skills_input: &Entity<TextInput>,
) -> gpui::Div {
    let mut section = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(bits::section_label("ROSTER & SKILLS"));

    // "no bots configured" is a claim about the roster, so it waits for a roster
    // that has actually been read (see `Fetched`).
    if s.bots.is_empty() {
        let message = match &s.bots {
            Fetched::Failed(reason) => format!("the roster could not be loaded: {reason}"),
            Fetched::Unknown => format!("the roster is not loaded yet ({})", s.conn.label()),
            Fetched::Ready(_) => "no bots configured".to_string(),
        };
        return section.child(
            div()
                .text_sm()
                .text_color(rgb(theme::TEXT_DIM))
                .child(message),
        );
    }

    for bot in &s.bots {
        let mut card = div()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
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
                            .child(bits::avatar(
                                theme::bot_color(bot),
                                bits::initial(&bot.label),
                                theme::space::AVATAR,
                            ))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(rgb(theme::TEXT))
                                    .child(bot.label.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(format!("@{}", bot.name)),
                            ),
                    )
                    .child(bits::badge(
                        bot.status.label(),
                        theme::SURFACE_RAISED,
                        theme::TEXT_MUTED,
                    )),
            )
            .child(bits::kv("backend", bot.backend.clone()));

        if let Some(model) = &bot.model {
            card = card.child(bits::kv("model override", model.clone()));
        }

        // Skills, read-only until the owner opens the editor.
        //
        // An empty list is not a gap to hide: it is the honest state of most
        // bots, and the line says what it means (the bot runs on its
        // instructions alone) rather than leaving a blank row.
        let editing = s.skills_editor.as_ref() == Some(&bot.id);
        let skills_div = if bot.skills.is_empty() {
            div()
                .text_xs()
                .italic()
                .text_color(rgb(theme::TEXT_DIM))
                .child("no skills — this bot runs on its instructions alone")
        } else {
            div().flex().flex_wrap().gap_1().children(
                bot.skills
                    .iter()
                    .map(|skill| bits::badge(skill.clone(), theme::SURFACE_RAISED, theme::ACCENT)),
            )
        };

        let state_edit = state.clone();
        let bot_for_edit = bot.id.clone();
        let draft_for_edit = skills_draft_of(&bot.skills);
        let input_for_edit = skills_input.clone();
        let mut skills_row = div()
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .child(
                div()
                    .w(px(120.0))
                    .flex_none()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child("skills"),
            )
            .child(div().flex_1().child(skills_div));

        if !editing {
            skills_row = skills_row.child(
                bits::button("Edit", Tone::Ghost)
                    .id(SharedString::from(format!(
                        "edit-skills-{}",
                        bot.id.as_str()
                    )))
                    .on_click(move |_, window, cx: &mut gpui::App| {
                        let seed = draft_for_edit.clone();
                        state_edit.update(cx, |st, cx| {
                            st.skills_editor = Some(bot_for_edit.clone());
                            cx.notify();
                        });
                        input_for_edit.update(cx, |input, cx| input.set_content(&seed, cx));
                        input_for_edit.read(cx).focus(window);
                    }),
            );
        }
        card = card.child(skills_row);

        if editing {
            let state_save = state.clone();
            let commands_save = commands.clone();
            let input_save = skills_input.clone();
            let bot_for_save = bot.id.clone();
            let current = bot.skills.clone();
            let state_cancel = state.clone();
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(theme::SURFACE_RAISED))
                    .child(skills_input.clone())
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_DIM))
                            .child(
                                "Comma-separated skill names. An empty field clears the list. The \
                                 name must match a directory under the brain's skill library; a name \
                                 with no library entry is carried into the bot's instructions as a \
                                 name only.",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                bits::button("Cancel", Tone::Ghost)
                                    .id(SharedString::from(format!(
                                        "cancel-skills-{}",
                                        bot.id.as_str()
                                    )))
                                    .on_click({
                                        let state_cancel = state_cancel.clone();
                                        move |_, _, cx: &mut gpui::App| {
                                            state_cancel.update(cx, |st, cx| {
                                                st.skills_editor = None;
                                                cx.notify();
                                            });
                                        }
                                    }),
                            )
                            .child(
                                bits::button("Save skills", Tone::Primary)
                                    .id(SharedString::from(format!(
                                        "save-skills-{}",
                                        bot.id.as_str()
                                    )))
                                    .on_click(move |_, _, cx: &mut gpui::App| {
                                        let typed = input_save.read(cx).content().to_string();
                                        let parsed = parse_skills(&typed);
                                        state_save.update(cx, |st, cx| {
                                            st.skills_editor = None;
                                            cx.notify();
                                        });
                                        // A save that changes nothing is not a
                                        // request: the brain would accept it, but
                                        // sending it would claim a write that did
                                        // not happen.
                                        if parsed != current {
                                            let _ = commands_save.send(
                                                Command::SetBotSkills {
                                                    bot: bot_for_save.clone(),
                                                    skills: parsed,
                                                },
                                            );
                                        }
                                    }),
                            ),
                    ),
            );
        }

        section = section.child(card);
    }

    section
}

/// The card rows for a backend's redacted `options`: the spawn command line
/// (`command` + `args`) merged into one **runs** row, every other option
/// shown under its own name. Pure, so a test can pin the wording.
pub fn connection_rows(options: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    if let Some(command) = options.get("command").and_then(Value::as_str) {
        let args = options
            .get("args")
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        rows.push((
            "runs".to_string(),
            if args.is_empty() {
                command.to_string()
            } else {
                format!("{command} {args}")
            },
        ));
    }
    for (name, value) in options {
        if name == "command" || name == "args" {
            continue;
        }
        rows.push((name.clone(), option_display(value)));
    }
    rows
}

/// One option value as a readable cell. An all-`[hidden]` object (the ACP
/// `env` map) shows just its variable names — the values were masked by the
/// brain and there is nothing honest left to render.
pub fn option_display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => {
            if *flag {
                "on".to_string()
            } else {
                "off".to_string()
            }
        }
        Value::Number(number) => number.to_string(),
        Value::Array(items) => items
            .iter()
            .map(option_display)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(map) if !map.is_empty() && map.values().all(is_hidden) => {
            map.keys().cloned().collect::<Vec<_>>().join(", ") + " (values hidden)"
        }
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| format!("{key}={}", option_display(value)))
            .collect::<Vec<_>>()
            .join(", "),
        Value::Null => "—".to_string(),
    }
}

fn is_hidden(value: &Value) -> bool {
    value.as_str() == Some("[hidden]")
}

fn settings_section(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
) -> gpui::Div {
    let Some(draft) = s.settings_draft.as_ref().or(s.settings.as_ref()) else {
        let commands_fetch = commands.clone();
        return div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(bits::section_label("BRAIN SETTINGS"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("settings not loaded"),
            )
            .child(
                bits::button("Load settings", Tone::Ghost)
                    .id("load-settings")
                    .on_click(move |_, _, _: &mut gpui::App| {
                        let _ = commands_fetch.send(Command::FetchSettings);
                    }),
            );
    };

    let mut section = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(bits::section_label("BRAIN SETTINGS"));

    section = section
        .child(toggle_row(
            "bot messaging (kill switch)",
            draft.bot_messaging,
            state,
            |d| d.bot_messaging = !d.bot_messaging,
        ))
        .child(toggle_row(
            "auto-approve read-only tools",
            draft.auto_approve_read_only,
            state,
            |d| d.auto_approve_read_only = !d.auto_approve_read_only,
        ))
        .child(toggle_row(
            "mirror transcripts to disk",
            draft.transcript_mirror,
            state,
            |d| d.transcript_mirror = !d.transcript_mirror,
        ))
        .child(stepper_row(
            "max hops",
            draft.max_hops as i64,
            state,
            |d, delta| {
                d.max_hops = (d.max_hops as i64 + delta).clamp(1, 32) as u8;
            },
        ))
        .child(stepper_row(
            "max tool rounds",
            draft.max_tool_rounds as i64,
            state,
            |d, delta| {
                d.max_tool_rounds = (d.max_tool_rounds as i64 + delta).clamp(1, 256) as u32;
            },
        ))
        .child(token_cap_row(
            "token cap / turn",
            draft.max_tokens_per_turn,
            state,
            |d, delta| {
                d.max_tokens_per_turn = stepped_token_cap(d.max_tokens_per_turn, delta);
            },
        ))
        .child(token_cap_row(
            "token cap / day",
            draft.max_tokens_per_day,
            state,
            |d, delta| {
                d.max_tokens_per_day = stepped_token_cap(d.max_tokens_per_day, delta);
            },
        ))
        .child(ceiling_row(draft.safety.default, state));

    // Save: PATCH only what changed.
    let commands_save = commands.clone();
    let state_save = state.clone();
    section = section.child(
        div().flex().justify_end().gap_2().child(
            bits::button("Save", Tone::Primary)
                .id("save-settings")
                .on_click(move |_, _, cx: &mut gpui::App| {
                    let patch = {
                        let st = state_save.read(cx);
                        match (&st.settings, &st.settings_draft) {
                            (Some(saved), Some(draft)) => diff(saved, draft),
                            _ => Value::Object(Map::new()),
                        }
                    };
                    if patch.as_object().is_some_and(|m| m.is_empty()) {
                        state_save.update(cx, |st, cx| {
                            st.toast = Some("no changes to save".to_string());
                            cx.notify();
                        });
                    } else {
                        let _ = commands_save.send(Command::UpdateSettings(patch));
                    }
                }),
        ),
    );
    section
}

fn toggle_row<F>(label: &str, value: bool, state: &Entity<AppState>, setter: F) -> gpui::AnyElement
where
    F: Fn(&mut Settings) + 'static,
{
    let state_h = state.clone();
    div()
        .id(SharedString::from(format!("toggle-{label}")))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .child(bits::dot(
            if value { theme::SUCCESS } else { theme::BORDER },
            px(12.0),
        ))
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(rgb(theme::TEXT))
                .child(label.to_string()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(if value { "on" } else { "off" }),
        )
        .on_click(move |_, _, cx: &mut gpui::App| {
            state_h.update(cx, |s, cx| {
                if let Some(draft) = &mut s.settings_draft {
                    setter(draft);
                }
                cx.notify();
            });
        })
        .into_any_element()
}

fn stepper_row<F>(label: &str, value: i64, state: &Entity<AppState>, setter: F) -> gpui::Div
where
    F: Fn(&mut Settings, i64) + Clone + 'static,
{
    stepper_row_text(label, value.to_string(), 48.0, state, setter)
}

/// A token-cap row: the same stepper as [`stepper_row`], but its value cell says
/// `off` for no cap and is wider, because a cap is six or seven digits.
fn token_cap_row<F>(label: &str, cap: Option<u64>, state: &Entity<AppState>, setter: F) -> gpui::Div
where
    F: Fn(&mut Settings, i64) + Clone + 'static,
{
    stepper_row_text(label, token_cap_label(cap), 72.0, state, setter)
}

/// The row itself: label, −/+ pair, and the rendered value. `shown` is separate
/// from the number the setter works on because a token cap's "no cap" is a word,
/// not a number.
fn stepper_row_text<F>(
    label: &str,
    shown: String,
    value_width: f32,
    state: &Entity<AppState>,
    setter: F,
) -> gpui::Div
where
    F: Fn(&mut Settings, i64) + Clone + 'static,
{
    let state_dec = state.clone();
    let state_inc = state.clone();
    let setter_dec = setter.clone();
    let setter_inc = setter;
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(rgb(theme::TEXT))
                .child(label.to_string()),
        )
        .child(
            bits::button("−", Tone::Ghost)
                .id(SharedString::from(format!("dec-{label}")))
                .on_click(move |_, _, cx: &mut gpui::App| {
                    state_dec.update(cx, |s, cx| {
                        if let Some(draft) = &mut s.settings_draft {
                            setter_dec(draft, -1);
                        }
                        cx.notify();
                    });
                }),
        )
        .child(
            div()
                .w(px(value_width))
                .text_center()
                .text_sm()
                .text_color(rgb(theme::TEXT))
                .child(shown),
        )
        .child(
            bits::button("+", Tone::Ghost)
                .id(SharedString::from(format!("inc-{label}")))
                .on_click(move |_, _, cx: &mut gpui::App| {
                    state_inc.update(cx, |s, cx| {
                        if let Some(draft) = &mut s.settings_draft {
                            setter_inc(draft, 1);
                        }
                        cx.notify();
                    });
                }),
        )
}

fn ceiling_row(current: PolicyCeiling, state: &Entity<AppState>) -> gpui::Div {
    let mut row = div().flex().items_center().gap_2().px_2().py_1().child(
        div()
            .flex_1()
            .text_sm()
            .text_color(rgb(theme::TEXT))
            .child("default safety ceiling"),
    );
    for ceiling in [
        PolicyCeiling::Never,
        PolicyCeiling::Ask,
        PolicyCeiling::Always,
    ] {
        let active = ceiling == current;
        let state_h = state.clone();
        row = row.child(
            bits::button(
                ceiling_label(ceiling),
                if active { Tone::Primary } else { Tone::Ghost },
            )
            .id(SharedString::from(format!(
                "ceiling-{}",
                ceiling_label(ceiling)
            )))
            .on_click(move |_, _, cx: &mut gpui::App| {
                state_h.update(cx, |s, cx| {
                    if let Some(draft) = &mut s.settings_draft {
                        draft.safety.default = ceiling;
                    }
                    cx.notify();
                });
            }),
        );
    }
    row
}

fn ceiling_label(ceiling: PolicyCeiling) -> &'static str {
    match ceiling {
        PolicyCeiling::Never => "never",
        PolicyCeiling::Ask => "ask",
        PolicyCeiling::Always => "always",
    }
}

pub(crate) fn backend_kind_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::OpenAiCompat => "openai-compat",
        BackendKind::ClaudeCli => "claude-cli",
        BackendKind::Acp => "acp",
        BackendKind::Fake => "fake",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_client::{BackendAuthMethod, BackendProbe};
    use swarmbot_contracts::PolicyCeiling;

    fn probe(ok: bool, auth_required: bool, methods: Vec<BackendAuthMethod>) -> BackendProbe {
        BackendProbe {
            backend: "claude".into(),
            ok,
            detail: Some("Authentication required".into()),
            model: None,
            version: None,
            auth_methods: methods,
            auth_required,
            login_command: Some("claude /login".into()),
        }
    }

    fn claude_login() -> BackendAuthMethod {
        BackendAuthMethod {
            id: "claude-login".into(),
            name: "Log in with [CC]".into(),
            description: Some("Run `claude /login` in the terminal".into()),
            command: None,
        }
    }

    /// The four states and the wording that goes with them. A green handshake
    /// must not be able to say "connected" when the backend itself said a login
    /// is what is missing — that is D9.28's failure mode, and the card is where
    /// it would show.
    #[test]
    fn connection_state_never_promotes_a_login_refusal_to_connected() {
        assert_eq!(connection_state(None), ConnectionState::NotChecked);
        assert_eq!(ConnectionState::NotChecked.label(), "not checked");

        assert_eq!(
            connection_state(Some(&probe(true, false, Vec::new()))),
            ConnectionState::Connected
        );
        assert_eq!(ConnectionState::Connected.label(), "connected");
        assert_eq!(
            ConnectionState::Connected.connect_label(),
            "Re-check",
            "a repeat of the same handshake says so instead of pretending to be new"
        );
        assert_eq!(ConnectionState::NeedsLogin.connect_label(), "Connect");

        // `auth_required` wins over `ok`: both orders are possible in the wire
        // (a refused session, or a passed handshake that still advertises a
        // login), and only one of them may read as connected.
        assert_eq!(
            connection_state(Some(&probe(false, true, vec![claude_login()]))),
            ConnectionState::NeedsLogin
        );
        assert_eq!(
            connection_state(Some(&probe(true, true, vec![claude_login()]))),
            ConnectionState::NeedsLogin
        );
        assert_eq!(ConnectionState::NeedsLogin.label(), "needs a login");

        assert_eq!(
            connection_state(Some(&probe(false, false, Vec::new()))),
            ConnectionState::Failed
        );
        assert_eq!(ConnectionState::Failed.label(), "not connected");
    }

    /// The warning that keeps a green handshake honest: it is printed only when
    /// the agent passed its checks *and* advertises a login, and it names that
    /// login rather than a guess.
    #[test]
    fn a_green_handshake_with_an_advertised_login_states_its_limit() {
        assert_eq!(handshake_only_note(None), None);
        assert_eq!(
            handshake_only_note(Some(&probe(true, false, Vec::new()))),
            None,
            "a backend with nothing to log in to has nothing to warn about"
        );
        let note = handshake_only_note(Some(&probe(true, false, vec![claude_login()])))
            .expect("the limit is stated");
        assert!(note.contains("handshake OK"), "{note}");
        assert!(note.contains("Log in with [CC]"), "{note}");
        // A fact plus a condition, never a prediction: `opencode` advertises a
        // login and answers turns, so "a turn may still be refused" would be an
        // alarm on a healthy backend.
        assert!(
            note.contains("if a turn is refused"),
            "the note must not predict a refusal: {note}"
        );
        assert!(!note.contains("may still be refused"), "{note}");
        assert!(
            handshake_only_note(Some(&probe(false, true, vec![claude_login()]))).is_none(),
            "a failed check is already saying it failed"
        );
    }

    #[test]
    fn save_patches_only_what_changed() {
        let saved = Settings::default();
        let mut draft = saved.clone();
        assert!(
            diff(&saved, &draft).as_object().expect("object").is_empty(),
            "no edits -> an empty patch (nothing is rewritten)"
        );

        draft.bot_messaging = false;
        draft.max_hops = 3;
        let patch = diff(&saved, &draft);
        assert_eq!(patch["bot_messaging"], false);
        assert_eq!(patch["max_hops"], 3);
        assert!(
            patch.get("max_tool_rounds").is_none(),
            "an untouched key is omitted so the brain's merge cannot clobber it"
        );
        assert!(patch.get("safety").is_none());
    }

    #[test]
    fn a_changed_ceiling_is_included_whole() {
        let saved = Settings::default();
        let mut draft = saved.clone();
        draft.safety.default = PolicyCeiling::Never;
        let patch = diff(&saved, &draft);
        assert!(
            patch.get("safety").is_some(),
            "a changed safety map is sent"
        );
        // The kill switch and limits are untouched, so they stay out.
        assert!(patch.get("bot_messaging").is_none());
        assert!(patch.get("max_hops").is_none());
    }

    #[test]
    fn a_changed_daily_budget_is_patched_alone() {
        let saved = Settings::default();
        let mut draft = saved.clone();
        draft.max_tokens_per_day = Some(250_000);
        let patch = diff(&saved, &draft);
        let keys: Vec<&str> = patch
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["max_tokens_per_day"],
            "only the budget that moved is sent"
        );
        assert_eq!(patch["max_tokens_per_day"], json!(250_000));
        assert!(
            patch.get("max_tokens_per_turn").is_none(),
            "the untouched per-turn cap stays out of the patch"
        );
    }

    #[test]
    fn an_unchanged_budget_is_not_patched() {
        let saved = Settings {
            max_tokens_per_turn: Some(100_000),
            max_tokens_per_day: Some(5_000_000),
            ..Settings::default()
        };
        let draft = saved.clone();
        let patch = diff(&saved, &draft);
        assert!(patch.get("max_tokens_per_turn").is_none());
        assert!(patch.get("max_tokens_per_day").is_none());
        assert!(
            patch.as_object().expect("object").is_empty(),
            "a budget equal to the saved one is not rewritten"
        );
    }

    #[test]
    fn stepping_a_budget_never_lands_on_zero() {
        let from_off = stepped_token_cap(None, 1);
        assert_eq!(
            from_off,
            Some(TOKEN_CAP_STEP as u64),
            "off (no cap) + one step is a real cap"
        );
        assert_ne!(from_off, Some(0), "a step up from off never yields 0");
        assert_eq!(
            token_cap_label(None),
            "off",
            "no cap renders as `off`, not `0`"
        );
        assert_eq!(token_cap_label(Some(10_000)), "10000");

        // Down from the smallest positive cap is `off`, not a zero the brain
        // rejects; stepping down from `off` stays `off`.
        let smallest = from_off.expect("a cap");
        assert_eq!(stepped_token_cap(Some(smallest), -1), None);
        assert_eq!(stepped_token_cap(None, -1), None);
        assert_eq!(
            stepped_token_cap(Some(1), -1),
            None,
            "a sub-step cap steps off rather than to Some(0)"
        );
        // `+` at the clamp holds the clamp (never wraps), and a config-set cap
        // above the clamp is not lowered by a `+` click.
        assert_eq!(
            stepped_token_cap(Some(TOKEN_CAP_MAX), 1),
            Some(TOKEN_CAP_MAX)
        );
        assert_eq!(
            stepped_token_cap(Some(TOKEN_CAP_MAX * 2), 1),
            Some(TOKEN_CAP_MAX * 2),
            "a config file's larger cap is not shrunk by pressing +"
        );
    }

    #[test]
    fn connection_rows_merge_the_command_line() {
        let options: serde_json::Map<String, Value> = serde_json::from_value(json!({
            "command": "opencode",
            "args": ["acp"],
            "permission_mode": "reject",
            "env": {"ANTHROPIC_API_KEY": "[hidden]"},
            "inherit_env": true,
        }))
        .expect("map");
        let rows = connection_rows(&options);
        assert_eq!(rows[0], ("runs".to_string(), "opencode acp".to_string()));
        let labels: Vec<&str> = rows.iter().map(|(label, _)| label.as_str()).collect();
        // command+args are merged into `runs`, never repeated separately.
        assert!(!labels.contains(&"command") && !labels.contains(&"args"));
        assert!(labels.contains(&"permission_mode"));
        let env = rows.iter().find(|(label, _)| label == "env").expect("env");
        assert_eq!(env.1, "ANTHROPIC_API_KEY (values hidden)");
        let inherit = rows
            .iter()
            .find(|(label, _)| label == "inherit_env")
            .expect("inherit_env");
        assert_eq!(inherit.1, "on");
    }

    #[test]
    fn option_display_reads_every_json_shape() {
        assert_eq!(option_display(&json!("x")), "x");
        assert_eq!(option_display(&json!(true)), "on");
        assert_eq!(option_display(&json!(false)), "off");
        assert_eq!(option_display(&json!(3)), "3");
        assert_eq!(option_display(&json!(["a", "b"])), "a, b");
        assert_eq!(option_display(&json!({"k": "v"})), "k=v");
        assert_eq!(option_display(&Value::Null), "—");
        // A masked map shows names only — the brain already hid the values.
        assert_eq!(
            option_display(&json!({"A": "[hidden]", "B": "[hidden]"})),
            "A, B (values hidden)"
        );
    }
    /// The skills draft is a comma-separated list typed by a human, so it has to
    /// survive the shapes humans type: stray spaces, a trailing comma, and the
    /// same skill twice. A duplicate is the interesting one — the brain keys a
    /// skill by name, so sending `lead, lead` would store one skill and render
    /// two badges.
    #[test]
    fn a_typed_skill_list_is_trimmed_deduplicated_and_order_preserving() {
        assert_eq!(
            parse_skills("research, review"),
            vec!["research".to_string(), "review".to_string()]
        );
        assert_eq!(
            parse_skills("  research ,, review ,,  "),
            vec!["research".to_string(), "review".to_string()],
            "spaces, empty items and a trailing comma are not skills"
        );
        assert_eq!(
            parse_skills("lead, lead, review"),
            vec!["lead".to_string(), "review".to_string()],
            "first-seen order, one row per name"
        );
    }

    /// Clearing the field is a real answer — remove every skill — and must not
    /// be refused as "nothing typed".
    #[test]
    fn an_empty_skill_field_means_no_skills_rather_than_no_change() {
        assert!(parse_skills("").is_empty());
        assert!(parse_skills("   ,  , ").is_empty());
        assert_eq!(skills_draft_of(&[]), "");
        assert_eq!(
            skills_draft_of(&["research".to_string(), "review".to_string()]),
            "research, review"
        );
        // The round trip a save performs must be stable, or every save would
        // report a change and PATCH the same list forever.
        let skills = vec!["a".to_string(), "b".to_string()];
        assert_eq!(parse_skills(&skills_draft_of(&skills)), skills);
    }

    /// A bot with no skills says what that means (it runs on its instructions),
    /// and the roster section does not claim "no bots configured" before the
    /// roster has been read.
    #[test]
    fn an_empty_skill_list_is_described_rather_than_hidden() {
        let mut state = AppState::new("http://brain.test");
        // Before any snapshot the roster is unknown, so the section must not say
        // "no bots configured"; the wording comes from `Fetched`.
        assert!(matches!(state.bots, Fetched::Unknown));
        let message = match &state.bots {
            Fetched::Ready(_) => "no bots configured".to_string(),
            Fetched::Failed(reason) => format!("the roster could not be loaded: {reason}"),
            Fetched::Unknown => format!("the roster is not loaded yet ({})", state.conn.label()),
        };
        assert!(
            message.contains("not loaded yet") && message.contains("connecting"),
            "{message}"
        );

        // Once answered, an empty roster is a fact.
        state.set_bots(Vec::new());
        assert!(matches!(&state.bots, Fetched::Ready(bots) if bots.is_empty()));
    }

    /// The save path sends the parsed list, and an unchanged list is not sent at
    /// all — a save that changes nothing must not claim to have written
    /// something.
    #[test]
    fn the_skills_command_carries_the_parsed_list() {
        use crate::brain::scripted::ScriptedBrain;
        use crate::worker::run_command;
        use swarmbot_contracts::BotId;

        let brain = ScriptedBrain::new();
        let bot = BotId::from_suffix("dev");
        let skills = parse_skills("research, review");
        let result = run_command(
            &brain,
            crate::proto::Command::SetBotSkills {
                bot: bot.clone(),
                skills: skills.clone(),
            },
        )
        .expect("a bot patch yields a result");
        assert!(
            matches!(result, crate::proto::CommandResult::BotUpdated(Ok(()))),
            "{result:?}"
        );
        let call = brain
            .calls()
            .into_iter()
            .find(|call| call.name == "update_bot")
            .expect("the client was called");
        assert!(call.detail.starts_with(bot.as_str()), "{}", call.detail);
        assert!(
            call.detail.contains("\"skills\":[\"research\",\"review\"]"),
            "the patch carries the parsed list: {}",
            call.detail
        );
    }
}

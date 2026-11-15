//! The cards that need a human: approvals, questions/forms, secrets and
//! hand-offs. Each card in the transcript is a [`CardView`] entity so its field
//! inputs keep focus and content across re-renders (a streaming transcript
//! re-renders often; recreating a text field each time would lose the owner's
//! half-typed answer).
//!
//! The honesty rule from [`../../../docs/ui.md`] §3 is enforced here: **a card's
//! state comes from `RuntimeRequest.status`, not from whether a button was
//! clicked.** Buttons are offered only while the request is `Pending`; once it is
//! `Answered`/`Expired`/`Cancelled` the card shows the outcome and stops offering
//! them, so an expiry looks like an expiry.

use std::collections::HashMap;
use std::sync::mpsc::Sender;

use chrono::Utc;
use gpui::{ClickEvent, Context, Entity, SharedString, Window, div, prelude::*, px, rgb};
use swarmbot_contracts::{
    Decision, EntryId, FieldKind, FormField, RequestId, RequestKind, RequestStatus, Responder,
};

use crate::proto::Command;
use crate::reducer;
use crate::state::AppState;
use crate::theme;
use crate::widgets::bits::{self, Tone};
use crate::widgets::text_input::TextInput;

/// One interactive card. Owns a text input per free-form field.
pub struct CardView {
    request_id: RequestId,
    entry_id: EntryId,
    /// (field name, its input) for text/multiline/secret fields.
    inputs: Vec<(String, Entity<TextInput>)>,
    /// Selected choice index / boolean value for choice/boolean fields.
    choices: HashMap<String, usize>,
    state: Entity<AppState>,
    commands: Sender<Command>,
}

impl CardView {
    pub fn new(
        request_id: RequestId,
        entry_id: EntryId,
        fields: &[FormField],
        state: Entity<AppState>,
        commands: Sender<Command>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut inputs = Vec::new();
        let mut choices = HashMap::new();
        for field in fields {
            match field.kind {
                FieldKind::Text | FieldKind::Multiline | FieldKind::Secret => {
                    let multiline = field.kind == FieldKind::Multiline;
                    let secure = field.kind == FieldKind::Secret;
                    let placeholder = field.default.clone().unwrap_or_default();
                    let input = cx.new(|cx| {
                        let mut ti = TextInput::new(&placeholder, cx);
                        if multiline {
                            ti = ti.multiline();
                        }
                        if secure {
                            ti = ti.secure();
                        }
                        ti
                    });
                    inputs.push((field.name.clone(), input));
                }
                FieldKind::Choice => {
                    choices.insert(field.name.clone(), 0);
                }
                FieldKind::Boolean => {
                    // 0 = false, 1 = true; default from the field's default text.
                    let on = matches!(field.default.as_deref(), Some("true") | Some("yes"));
                    choices.insert(field.name.clone(), usize::from(on));
                }
            }
        }
        Self {
            request_id,
            entry_id,
            inputs,
            choices,
            state,
            commands,
        }
    }

    fn send(&self, decision: Option<Decision>, answer: Option<serde_json::Value>) {
        let _ = self.commands.send(reducer::respond_command(
            self.request_id.clone(),
            decision,
            answer,
        ));
    }

    fn on_allow_once(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.send(Some(Decision::AllowOnce), None);
        cx.notify();
    }

    fn on_allow_always(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.send(Some(Decision::AllowAlways), None);
        cx.notify();
    }

    fn on_deny(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.send(Some(Decision::Deny), None);
        cx.notify();
    }

    fn on_done(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.send(None, Some(serde_json::json!({ "done": true })));
        cx.notify();
    }

    fn on_skip(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.send(None, Some(serde_json::json!({ "done": false })));
        cx.notify();
    }

    /// Collect every field's current value and answer the request with it.
    fn on_submit(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let request = self.state.read(cx).request(&self.request_id).cloned();
        let Some(request) = request else {
            return;
        };
        let mut values: HashMap<String, String> = HashMap::new();
        for (name, input) in &self.inputs {
            values.insert(name.clone(), input.read(cx).content().to_string());
        }
        for field in &request.fields {
            match field.kind {
                FieldKind::Choice => {
                    let idx = self.choices.get(&field.name).copied().unwrap_or(0);
                    if let Some(choice) = field.choices.get(idx) {
                        values.insert(field.name.clone(), choice.clone());
                    }
                }
                FieldKind::Boolean => {
                    let on = self.choices.get(&field.name).copied().unwrap_or(0) == 1;
                    values.insert(field.name.clone(), on.to_string());
                }
                _ => {}
            }
        }
        let answer = reducer::build_answer(&request.fields, &values);
        self.send(None, Some(answer));
        cx.notify();
    }

    fn select_choice(&mut self, field: String, index: usize, cx: &mut Context<Self>) {
        self.choices.insert(field, index);
        cx.notify();
    }

    fn toggle_bool(&mut self, field: String, cx: &mut Context<Self>) {
        let next = if self.choices.get(&field).copied().unwrap_or(0) == 1 {
            0
        } else {
            1
        };
        self.choices.insert(field, next);
        cx.notify();
    }
}

/// `mm:ss` until `expires_at`, or "expired"/"no expiry".
fn countdown(expires_at: Option<chrono::DateTime<Utc>>) -> String {
    countdown_from(Utc::now(), expires_at)
}

/// [`countdown`] with the clock passed in, so its three states are assertable
/// without waiting for one to tick over.
fn countdown_from(now: chrono::DateTime<Utc>, expires_at: Option<chrono::DateTime<Utc>>) -> String {
    let Some(at) = expires_at else {
        return "no expiry".to_string();
    };
    let remaining = at.signed_duration_since(now);
    let secs = remaining.num_seconds();
    if secs <= 0 {
        return "expired".to_string();
    }
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn outcome_line(status: RequestStatus, request: &swarmbot_contracts::RuntimeRequest) -> String {
    match status {
        RequestStatus::Answered => match (&request.responded_by, &request.decision) {
            (Some(Responder::Rule { .. }), _) => "answered automatically by a rule".to_string(),
            (_, Some(Decision::AllowOnce)) => "allowed once".to_string(),
            (_, Some(Decision::AllowAlways)) => "allowed (and always from now on)".to_string(),
            (_, Some(Decision::Deny)) => "denied".to_string(),
            _ => "answered".to_string(),
        },
        RequestStatus::Expired => "expired — nobody answered in time".to_string(),
        RequestStatus::Cancelled => match &request.responded_by {
            Some(Responder::System { reason }) => format!("cancelled: {reason}"),
            _ => "cancelled".to_string(),
        },
        RequestStatus::Pending => String::new(),
    }
}

/// The colour the card's outcome sentence is drawn in.
///
/// These are `*_TEXT` tokens, not the vivid status hues: an outcome line is a
/// 14px sentence inside a raised card, and `SUCCESS`/`WARNING` are fills — 2.2:1
/// and 1.8:1 on that card surface — so "allowed once" in green was the least
/// readable thing on the card. The hue still carries the meaning through the
/// darkened twin, and [`tests::an_outcome_line_is_drawn_in_a_readable_token`]
/// pins it.
fn outcome_color(status: RequestStatus) -> u32 {
    match status {
        RequestStatus::Answered => theme::SUCCESS_TEXT,
        RequestStatus::Expired => theme::WARNING_TEXT,
        RequestStatus::Cancelled => theme::TEXT_DIM,
        RequestStatus::Pending => theme::TEXT_MUTED,
    }
}

impl gpui::Render for CardView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (request, bot_meta) = {
            let st = self.state.read(cx);
            let Some(request) = st.request(&self.request_id).cloned() else {
                // The request is gone (deleted conversation, or not yet synced).
                return div()
                    .text_sm()
                    .text_color(rgb(theme::TEXT_DIM))
                    .child("(card unavailable)")
                    .into_any_element();
            };
            let bot = st.bot(&request.bot_id);
            let handle = bot
                .map(|b| format!("@{}", b.name))
                .unwrap_or_else(|| format!("@{}", request.bot_id.as_str()));
            let color = bot
                .map(theme::bot_color)
                .unwrap_or_else(|| theme::bot_color_from_id(request.bot_id.as_str()));
            (request, (handle, color))
        };
        let pending = request.status == RequestStatus::Pending;
        let eid = self.entry_id.as_str().to_string();

        let mut card = div()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE_RAISED))
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .overflow_hidden()
            // Bot attribution + Title.
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        // The pill may shrink (a bot handle can be long), so it and
                        // its label both carry `min_w_0`/`truncate`; the title next
                        // to it takes the remaining width and does the same. The
                        // card clips at its own rounded edge as a last resort.
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .min_w_0()
                            .px_2()
                            .py(px(1.0))
                            .rounded_full()
                            .bg(rgb(0xffffff))
                            .border_1()
                            .border_color(rgb(theme::BORDER))
                            .child(
                                div()
                                    .size(px(7.0))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(bot_meta.1),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(bot_meta.1)
                                    .truncate()
                                    .child(bot_meta.0),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgb(theme::TEXT))
                            .truncate()
                            .child(request.title()),
                    ),
            );

        // Target / reason / prompt.
        if let Some(target) = &request.target {
            card = card.child(
                div()
                    .id(SharedString::from(format!("card-target-{eid}")))
                    .w_full()
                    .rounded_md()
                    .bg(rgb(theme::CODE_BG))
                    .px_2()
                    .py_1()
                    .font_family("DejaVu Sans Mono")
                    .text_size(px(12.5))
                    .text_color(rgb(theme::TEXT))
                    .overflow_x_scroll()
                    .child(target.clone()),
            );
        }
        if let Some(reason) = &request.reason {
            card = card.child(
                div()
                    .w_full()
                    .text_sm()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(reason.clone()),
            );
        }
        if let Some(prompt) = &request.prompt {
            card = card.child(
                div()
                    .w_full()
                    .text_sm()
                    .text_color(rgb(theme::TEXT))
                    .child(prompt.clone()),
            );
        }

        if pending {
            match request.kind {
                RequestKind::Approval => {
                    card = card
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::WARNING_TEXT))
                                .child(format!("expires in {}", countdown(request.expires_at))),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    bits::button("Allow once", Tone::Success)
                                        .id(SharedString::from(format!("{eid}-allow")))
                                        .on_click(cx.listener(Self::on_allow_once)),
                                )
                                .child(
                                    bits::button("Always allow", Tone::Primary)
                                        .id(SharedString::from(format!("{eid}-always")))
                                        .on_click(cx.listener(Self::on_allow_always)),
                                )
                                .child(
                                    bits::button("Deny", Tone::Danger)
                                        .id(SharedString::from(format!("{eid}-deny")))
                                        .on_click(cx.listener(Self::on_deny)),
                                ),
                        );
                }
                RequestKind::Handoff => {
                    card = card.child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                bits::button("Done", Tone::Primary)
                                    .id(SharedString::from(format!("{eid}-done")))
                                    .on_click(cx.listener(Self::on_done)),
                            )
                            .child(
                                bits::button("Skip", Tone::Ghost)
                                    .id(SharedString::from(format!("{eid}-skip")))
                                    .on_click(cx.listener(Self::on_skip)),
                            ),
                    );
                }
                RequestKind::Question | RequestKind::Form | RequestKind::Secret => {
                    // Free-form fields (their inputs are child entities).
                    for field in &request.fields {
                        card = card.child(self.render_field(field, cx));
                    }
                    card = card.child(
                        bits::button("Submit answer", Tone::Primary)
                            .id(SharedString::from(format!("{eid}-submit")))
                            .on_click(cx.listener(Self::on_submit)),
                    );
                }
            }
        } else {
            let line = outcome_line(request.status, &request);
            card = card.child(
                div()
                    .text_sm()
                    .text_color(rgb(outcome_color(request.status)))
                    .child(line),
            );
        }

        card.into_any_element()
    }
}

impl CardView {
    /// One field of a question/form/secret card: a label plus the right input.
    fn render_field(&mut self, field: &FormField, cx: &mut Context<Self>) -> gpui::Div {
        let label = div()
            .text_xs()
            .text_color(rgb(theme::TEXT_MUTED))
            .child(if field.required {
                format!("{} *", field.label)
            } else {
                field.label.clone()
            });
        let body = match field.kind {
            FieldKind::Choice => {
                let selected = self.choices.get(&field.name).copied().unwrap_or(0);
                let mut col = div().flex().flex_col().gap_1();
                for (i, choice) in field.choices.iter().enumerate() {
                    let name = field.name.clone();
                    let is_sel = i == selected;
                    col = col.child(
                        div()
                            .id(SharedString::from(format!(
                                "{}-{}-{i}",
                                self.entry_id, field.name
                            )))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .when(is_sel, |d| d.bg(rgb(theme::SURFACE_RAISED)))
                            .child(bits::dot(
                                if is_sel { theme::ACCENT } else { theme::BORDER },
                                px(10.0),
                            ))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(theme::TEXT))
                                    .child(choice.clone()),
                            )
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.select_choice(name.clone(), i, cx);
                            })),
                    );
                }
                col.into_any_element()
            }
            FieldKind::Boolean => {
                let on = self.choices.get(&field.name).copied().unwrap_or(0) == 1;
                let name = field.name.clone();
                div()
                    .id(SharedString::from(format!(
                        "{}-{}",
                        self.entry_id, field.name
                    )))
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .child(bits::dot(
                        if on { theme::SUCCESS } else { theme::BORDER },
                        px(12.0),
                    ))
                    .child(div().text_sm().text_color(rgb(theme::TEXT)).child(if on {
                        "yes"
                    } else {
                        "no"
                    }))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_bool(name.clone(), cx);
                    }))
                    .into_any_element()
            }
            _ => {
                // Text / multiline / secret: the cached input entity.
                let input = self
                    .inputs
                    .iter()
                    .find(|(n, _)| n == &field.name)
                    .map(|(_, e)| e.clone());
                match input {
                    Some(input) => div().child(input).into_any_element(),
                    None => div().into_any_element(),
                }
            }
        };
        div().flex().flex_col().gap_1().child(label).child(body)
    }
}

/// A non-interactive card shell, used for a card whose request has not arrived
/// yet (keeps the transcript honest instead of silently dropping the row).
pub fn placeholder(request_id: &RequestId) -> gpui::Div {
    div()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .p_3()
        .text_sm()
        .text_color(rgb(theme::TEXT_DIM))
        .child(format!("card {} (waiting for its request)", request_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// WCAG 2.x relative luminance, local to this file: the colour *pair* under
    /// test (a status token on a card surface) is chosen here, so the assertion
    /// has to compute the ratio itself rather than trust a shared helper.
    fn luminance(color: u32) -> f64 {
        let channel = |shift: u32| {
            let v = ((color >> shift) & 0xff) as f64 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }

    fn contrast(fg: u32, bg: u32) -> f64 {
        let (a, b) = (luminance(fg), luminance(bg));
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// The defect: a settled card said what happened in the vivid hue —
    /// `theme::SUCCESS` for "allowed once" (2.4:1 on the card) and
    /// `theme::WARNING` for "expired" (2.1:1) — so the one sentence a reader
    /// needs after the fact was the least legible line on the card.
    #[test]
    fn an_outcome_line_is_drawn_in_a_readable_token() {
        for (status, vivid) in [
            (RequestStatus::Answered, theme::SUCCESS),
            (RequestStatus::Expired, theme::WARNING),
        ] {
            let color = outcome_color(status);
            assert_ne!(color, vivid, "{status:?} still uses a fill hue as text");
            let ratio = contrast(color, theme::SURFACE_RAISED);
            assert!(
                ratio >= 4.5,
                "{status:?} on the card surface is {ratio:.2}:1, under the 4.5:1 bar"
            );
            // …and the old pairing is what fails that bar, which is why the
            // token changed rather than the surface.
            assert!(
                contrast(vivid, theme::SURFACE_RAISED) < 4.5,
                "{status:?}'s fill hue passes the text bar; the twin would be dead weight"
            );
        }
        // The two non-status outcomes keep their muted/dim tiers.
        assert_eq!(
            outcome_color(RequestStatus::Cancelled),
            theme::TEXT_DIM,
            "a cancelled card is quiet, not coloured"
        );
    }

    /// `countdown` is the only clock-dependent label on a card; its three states
    /// are the ones a card can be wedged in, so they are pinned here.
    #[test]
    fn a_countdown_names_itself_rather_than_printing_nothing() {
        let now = Utc
            .with_ymd_and_hms(2026, 11, 15, 9, 0, 0)
            .single()
            .expect("a real UTC time");

        // No expiry is a state, not an empty string.
        assert_eq!(countdown_from(now, None), "no expiry");
        // A minute and a half reads as mm:ss.
        assert_eq!(
            countdown_from(now, Some(now + chrono::Duration::seconds(90))),
            "1:30"
        );
        // A past expiry says so instead of counting negatively.
        assert_eq!(
            countdown_from(now, Some(now - chrono::Duration::seconds(5))),
            "expired"
        );
    }
}

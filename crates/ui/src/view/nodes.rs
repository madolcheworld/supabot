//! The Nodes screen: pairing, the node roster with effective capabilities, and
//! revoke.
//!
//! Two honesty rules shape it. The pairing code is shown large and copyable with
//! its expiry and the exact `swarmbot-node pair <brain> <code>` command, because
//! that is what the operator actually needs. And the audit log is **not**
//! invented: the public API does not expose a node's audit tail yet, so the panel
//! says "the audit log lives on the node" instead of rendering fake rows.
//!
//! Capabilities are shown as declared ∩ approved ([`swarmbot_contracts::NodeInfo::capabilities`]),
//! and a ceiling is never presented as raisable from here — it is the node's own.

use std::sync::mpsc::Sender;

use gpui::{App, ClipboardItem, Entity, SharedString, div, prelude::*, px, rgb};
use swarmbot_contracts::{PolicyCeiling, RootMode};

use crate::proto::Command;
use crate::state::AppState;
use crate::theme;
use crate::widgets::bits::{self, Tone};

pub fn render(s: &AppState, _state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
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
                .child("Nodes"),
        )
        .child(
            div()
                .id("nodes-scroll")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_4()
                .px_4()
                .py_3()
                .child(pairing_section(s, commands))
                .child(node_list(s, commands))
                .child(div().text_xs().text_color(rgb(theme::TEXT_DIM)).child(
                    "The audit log lives on the node; the public API does not expose it yet.",
                )),
        )
}

fn pairing_section(s: &AppState, commands: &Sender<Command>) -> gpui::Div {
    let mut section = div()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(bits::section_label("PAIR A NODE"));

    match &s.pairing {
        Some(pairing) => {
            let command = format!("swarmbot-node pair {} {}", s.brain_url, pairing.code);
            let code = pairing.code.clone();
            let command_clone = command.clone();
            let commands_code = commands.clone();
            section = section
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .text_3xl()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(rgb(theme::TEXT))
                                .child(pairing.code.clone()),
                        )
                        .child(
                            bits::button("Copy code", Tone::Ghost)
                                .id("copy-code")
                                .on_click(move |_, _, cx: &mut App| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                                }),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::WARNING))
                        .child(format!("expires {}", pairing.expires_at)),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .rounded_md()
                                .bg(rgb(theme::CODE_BG))
                                .px_2()
                                .py_1()
                                .font_family("DejaVu Sans Mono")
                                .text_size(px(12.0))
                                .text_color(rgb(theme::TEXT))
                                .child(command_clone.clone()),
                        )
                        .child(
                            bits::button("Copy command", Tone::Ghost)
                                .id("copy-command")
                                .on_click(move |_, _, cx: &mut App| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        command.clone(),
                                    ));
                                }),
                        ),
                )
                .child(
                    bits::button("New code", Tone::Primary)
                        .id("new-code")
                        .on_click(move |_, _, _: &mut App| {
                            let _ = commands_code.send(Command::Pairing);
                        }),
                );
        }
        None => {
            let commands_mint = commands.clone();
            section = section
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("No pending pairing code."),
                )
                .child(
                    bits::button("Create pairing code", Tone::Primary)
                        .id("mint-code")
                        .on_click(move |_, _, _: &mut App| {
                            let _ = commands_mint.send(Command::Pairing);
                        }),
                );
        }
    }
    section
}

fn node_list(s: &AppState, commands: &Sender<Command>) -> gpui::Div {
    let mut section = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(bits::section_label("PAIRED NODES"));

    if s.nodes.is_empty() {
        return section.child(
            div()
                .text_sm()
                .text_color(rgb(theme::TEXT_DIM))
                .child("no nodes paired yet"),
        );
    }

    for node in &s.nodes {
        let caps: Vec<String> = node.capabilities().as_slice().to_vec();
        let declared = node.declared.as_slice().len();
        let approved = node.approved.as_slice().len();
        let roots: Vec<String> = node
            .roots
            .iter()
            .map(|r| {
                format!(
                    "{} ({})",
                    r.path,
                    match r.mode {
                        RootMode::ReadOnly => "ro",
                        RootMode::ReadWrite => "rw",
                    }
                )
            })
            .collect();
        let ceilings: Vec<String> = node
            .policy
            .surfaces
            .iter()
            .map(|(surface, ceiling)| format!("{}: {}", surface, ceiling_label(*ceiling)))
            .collect();

        let commands_revoke = commands.clone();
        let node_id = node.id.clone();

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
                    .child(bits::dot(
                        if node.online {
                            theme::SUCCESS
                        } else {
                            theme::TEXT_DIM
                        },
                        px(9.0),
                    ))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgb(theme::TEXT))
                            .child(node.label.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_DIM))
                            .child(format!(
                                "{} · {}/{}",
                                node.hostname, node.platform, node.arch
                            )),
                    )
                    .child(div().flex_1())
                    .child(if node.revoked {
                        bits::badge("revoked", theme::DANGER, 0xffffff)
                    } else {
                        bits::badge(
                            if node.online { "online" } else { "offline" },
                            if node.online {
                                theme::SUCCESS
                            } else {
                                theme::TEXT_DIM
                            },
                            0x0b1410,
                        )
                    }),
            )
            .child(bits::kv(
                "capabilities",
                if caps.is_empty() {
                    "none effective".to_string()
                } else {
                    caps.join(", ")
                },
            ))
            .child(bits::kv(
                "declared / approved",
                format!("{declared} declared · {approved} approved"),
            ))
            .child(bits::kv(
                "roots",
                if roots.is_empty() {
                    "—".to_string()
                } else {
                    roots.join(", ")
                },
            ))
            .child(bits::kv(
                "ceiling",
                if ceilings.is_empty() {
                    format!("default {}", ceiling_label(node.policy.default))
                } else {
                    format!(
                        "default {} · {}",
                        ceiling_label(node.policy.default),
                        ceilings.join(", ")
                    )
                },
            ))
            .child(bits::kv(
                "version",
                format!("{} (protocol {})", node.version, node.protocol),
            ));

        if !node.revoked {
            card = card.child(
                div().flex().justify_end().child(
                    bits::button("Revoke", Tone::Danger)
                        .id(SharedString::from(format!("revoke-{}", node.id)))
                        .on_click(move |_, _, _: &mut App| {
                            let _ = commands_revoke.send(Command::RevokeNode(node_id.clone()));
                        }),
                ),
            );
        }
        section = section.child(card);
    }
    section
}

fn ceiling_label(ceiling: PolicyCeiling) -> &'static str {
    match ceiling {
        PolicyCeiling::Never => "never",
        PolicyCeiling::Ask => "ask",
        PolicyCeiling::Always => "always",
    }
}

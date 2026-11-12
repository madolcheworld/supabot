//! The delegation graph: the tree of [`Task`]s for the open conversation, which
//! is what makes "bots delegating to each other" visible at a glance.
//!
//! The layout is a pure DFS ([`layout`]) over `parent_task_id`, so a
//! `#[test]` can assert the shape (roots, depth, ordering) without a
//! window. Edges come from `Task.parent_task_id`; a hand-off's `requester` is
//! shown as the "from @bot" label on the node it produced. Clicking a node
//! selects it and fetches its [`swarmbot_contracts::TaskStep`]s, listed below.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;

use gpui::{Entity, SharedString, div, prelude::*, px, rgb};
use swarmbot_contracts::{Task, TaskId, TaskStatus};

use crate::proto::Command;
use crate::state::AppState;
use crate::theme;
use crate::widgets::bits;

/// One row of the flattened tree, in display order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutNode {
    pub task: TaskId,
    pub depth: usize,
}

/// Flatten a task forest into display order (DFS, roots and siblings by creation
/// time), guarding against a cycle so a malformed `parent_task_id` cannot loop.
pub fn layout(tasks: &[Task]) -> Vec<LayoutNode> {
    let by_id: HashMap<&TaskId, &Task> = tasks.iter().map(|t| (&t.id, t)).collect();
    let mut children: HashMap<Option<TaskId>, Vec<&Task>> = HashMap::new();
    for task in tasks {
        // A parent that is not in this set makes the task a root.
        let key = task
            .parent_task_id
            .as_ref()
            .filter(|p| by_id.contains_key(p))
            .cloned();
        children.entry(key).or_default().push(task);
    }
    for list in children.values_mut() {
        list.sort_by_key(|task| task.created_at);
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<(TaskId, usize)> = children
        .get(&None)
        .map(|roots| roots.iter().rev().map(|t| (t.id.clone(), 0)).collect())
        .unwrap_or_default();
    while let Some((id, depth)) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        out.push(LayoutNode {
            task: id.clone(),
            depth,
        });
        if let Some(kids) = children.get(&Some(id.clone())) {
            for kid in kids.iter().rev() {
                stack.push((kid.id.clone(), depth + 1));
            }
        }
    }
    out
}

fn status_color(status: TaskStatus) -> u32 {
    match status {
        TaskStatus::Queued => theme::TEXT_DIM,
        TaskStatus::Running => theme::ACCENT,
        TaskStatus::Succeeded => theme::SUCCESS,
        TaskStatus::Failed => theme::DANGER,
        TaskStatus::Cancelled => theme::TEXT_DIM,
        TaskStatus::Interrupted => theme::WARNING,
    }
}

/// The embeddable delegation section (used inside [`crate::view::side_panel`]
/// so the task tree and step detail live inside the right panel scroll area
/// without nested scroll containers).
pub fn render_section(
    s: &AppState,
    state: &Entity<AppState>,
    commands: &Sender<Command>,
) -> gpui::Div {
    let mut section = div()
        .w_full()
        .rounded_xl()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(bits::section_label("DELEGATION"));

    let Some(conversation) = s.open_conversation() else {
        return section.child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .child("no conversation open"),
        );
    };
    let tasks = s.tasks_for(&conversation.id);
    if tasks.is_empty() {
        return section.child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .child("no delegated work yet"),
        );
    }

    let flat = layout(&tasks);
    let mut tree = div().w_full().flex().flex_col().gap_0p5();

    for node in &flat {
        let Some(task) = tasks.iter().find(|t| t.id == node.task) else {
            continue;
        };
        let color = status_color(task.status);
        let selected = s.open_task.as_ref() == Some(&task.id);
        let owner = s
            .bot(&task.owner)
            .map(|b| b.label.clone())
            .unwrap_or_else(|| task.owner.as_str().to_string());
        let from = task
            .requester
            .as_ref()
            .and_then(|id| s.bot(id))
            .map(|b| format!("from @{}", b.name));

        let state_h = state.clone();
        let commands_h = commands.clone();
        let task_id = task.id.clone();
        let task_id2 = task.id.clone();

        let row = div()
            .id(SharedString::from(format!("task-{}", task.id)))
            .w_full()
            .flex()
            .items_start()
            .gap_2()
            .pl(px(node.depth as f32 * 14.0))
            .px_1()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .hover(|h| h.bg(rgb(theme::SURFACE_RAISED)))
            .when(selected, |d| d.bg(rgb(theme::SURFACE_RAISED)))
            // Connector for depth, so the tree reads as a tree.
            .when(node.depth > 0, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_color(rgb(theme::BORDER))
                        .text_xs()
                        .child("└"),
                )
            })
            .child(div().flex_none().child(bits::dot(color, px(9.0))))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .w_full()
                            .text_sm()
                            .text_color(rgb(theme::TEXT))
                            .child(task.title.clone()),
                    )
                    .child(
                        div()
                            .w_full()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_DIM))
                            .truncate()
                            .child(format!(
                                "@{owner} · {}{}",
                                task.status.label(),
                                from.map(|f| format!(" · {f}")).unwrap_or_default()
                            )),
                    )
                    .when(task.restarts > 0, |d| {
                        d.child(
                            div()
                                .w_full()
                                .text_xs()
                                .text_color(rgb(theme::WARNING))
                                .child(format!("restarted {}", task.restarts)),
                        )
                    })
                    .child(
                        div()
                            .w_full()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_DIM))
                            .child(format!("hop {}", node.depth)),
                    ),
            )
            .on_click(move |_, _, cx: &mut gpui::App| {
                state_h.update(cx, |st, cx| {
                    st.open_task = Some(task_id.clone());
                    cx.notify();
                });
                let _ = commands_h.send(Command::TaskDetail(task_id2.clone()));
            });
        tree = tree.child(row);
    }

    section = section.child(tree);

    // Steps of the selected task, if fetched.
    if let Some(open_task) = &s.open_task
        && tasks.iter().any(|t| &t.id == open_task)
    {
        let steps = s.steps.get(open_task);
        let mut step_panel = div()
            .id("graph-steps")
            .w_full()
            .border_t_1()
            .border_color(rgb(theme::BORDER))
            .pt_2()
            .flex()
            .flex_col()
            .gap_1()
            .child(bits::section_label("STEPS"));
        match steps {
            Some(steps) if !steps.is_empty() => {
                for step in steps {
                    step_panel = step_panel.child(
                        div()
                            .w_full()
                            .flex()
                            .gap_2()
                            .text_xs()
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(rgb(theme::TEXT_DIM))
                                    .child(step_kind_mark(step.kind)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(step.text.clone()),
                            ),
                    );
                }
            }
            Some(_) => {
                step_panel = step_panel.child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child("no steps recorded"),
                );
            }
            None => {
                step_panel = step_panel.child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child("loading steps…"),
                );
            }
        }
        // Cancel a task that is not terminal.
        if let Some(task) = s.task(open_task)
            && !task.is_terminal()
        {
            let commands_h = commands.clone();
            let id = open_task.clone();
            step_panel = step_panel.child(
                bits::button("Cancel task", crate::widgets::bits::Tone::Danger)
                    .id(SharedString::from(format!("cancel-{open_task}")))
                    .on_click(move |_, _, _: &mut gpui::App| {
                        let _ = commands_h.send(Command::CancelTask(id.clone()));
                    }),
            );
        }
        section = section.child(step_panel);
    }

    section
}

/// The standalone right-hand delegation panel for the open conversation.
pub fn render(s: &AppState, state: &Entity<AppState>, commands: &Sender<Command>) -> gpui::Div {
    div()
        .w(theme::space::GRAPH_W)
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::SURFACE))
        .border_l_1()
        .border_color(rgb(theme::BORDER))
        .overflow_hidden()
        .child(
            div()
                .id("graph-panel-scroll")
                .flex_1()
                .overflow_y_scroll()
                .p_3()
                .child(render_section(s, state, commands)),
        )
}

fn step_kind_mark(kind: swarmbot_contracts::StepKind) -> &'static str {
    use swarmbot_contracts::StepKind;
    match kind {
        StepKind::Message => ">",
        StepKind::Reasoning => "~",
        StepKind::Tool => "#",
        StepKind::Update => "^",
        StepKind::Result => "=",
        StepKind::Notice => "•",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::{BotId, ConversationId, Task};

    fn task(id: &str, parent: Option<&str>) -> Task {
        let mut task = Task::own(BotId::new(), ConversationId::new(), id, "brief");
        task.id = TaskId::from_suffix(id);
        task.parent_task_id = parent.map(TaskId::from_suffix);
        task
    }

    #[test]
    fn layout_flattens_the_delegation_tree_in_dfs_order() {
        let tasks = vec![
            task("root", None),
            task("child", Some("root")),
            task("grandchild", Some("child")),
            task("other", None),
        ];
        let flat = layout(&tasks);
        assert_eq!(flat.len(), 4);
        let depths: Vec<usize> = flat.iter().map(|n| n.depth).collect();
        assert_eq!(depths, vec![0, 1, 2, 0], "hop depth follows the tree");
        let ids: Vec<String> = flat.iter().map(|n| n.task.as_str().to_string()).collect();
        assert_eq!(ids[0], "tsk_root");
        assert_eq!(ids[2], "tsk_grandchild");
    }

    #[test]
    fn an_orphan_whose_parent_is_absent_becomes_a_root() {
        let flat = layout(&[task("x", Some("missing"))]);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].depth, 0);
    }

    #[test]
    fn a_cycle_terminates_instead_of_looping() {
        // a→b→a: neither is a real root, so nothing is emitted, but `layout`
        // must return rather than spin forever.
        let flat = layout(&[task("a", Some("b")), task("b", Some("a"))]);
        assert!(flat.is_empty());
    }
}

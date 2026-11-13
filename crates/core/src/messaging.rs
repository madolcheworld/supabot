//! Bot-to-bot messaging: `message_bot`, `post_to_group`, `handoff_task`, `react`.
//!
//! Three rules keep a team from turning into noise:
//!
//! * **group work stays in its group** — from a group turn, a message to a
//!   fellow member is posted *in the group*, not into a private DM. A team that
//!   splits into side channels is a team the owner cannot follow.
//! * **the hop limit is a hard stop** — a chain longer than `max_hops` is
//!   stopped with an event line rather than allowed to oscillate.
//! * **a bot never interrupts a bot** — deliveries queue; only the owner can
//!   stop a turn.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use tracing::{debug, warn};

use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::conversation::{Conversation, ConversationKind};
use swarmbot_contracts::delivery::{Delivery, Urgency};
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, ConversationId, EntryId, TurnId};
use swarmbot_contracts::repo::Persistence;
use swarmbot_contracts::task::{Task, TaskStatus};
use swarmbot_contracts::tool::ToolResult;

use crate::runners::RunnerMap;
use crate::settings::SettingsHandle;
use crate::tasks::BackgroundTasks;
use crate::transcript::Transcript;

/// Who is sending, and from which turn.
#[derive(Clone, Debug)]
pub struct SendContext {
    pub from: Bot,
    /// The conversation the sending turn belongs to.
    pub conversation: Conversation,
    /// How many bot-to-bot hops this chain has already made.
    pub hops: u8,
    /// The entry that triggered it, so a reply can point back.
    pub source_entry: Option<EntryId>,
}

pub struct Messaging {
    store: Arc<dyn Persistence>,
    transcript: Arc<Transcript>,
    settings: Arc<SettingsHandle>,
    runners: Arc<RunnerMap>,
    tasks: Arc<BackgroundTasks>,
    turn_targets: Mutex<HashMap<TurnId, HashSet<BotId>>>,
}

/// What a send produced, for the tool result and for tests.
#[derive(Clone, Debug)]
pub struct Sent {
    /// Where the message was posted (the group, or the recipient's DM).
    pub conversation: ConversationId,
    pub entry: Option<EntryId>,
    /// True when the recipient was woken.
    pub woke: bool,
    /// True when the hop limit stopped the chain.
    pub stopped: bool,
    pub task: Option<Task>,
}

impl Messaging {
    pub fn new(
        store: Arc<dyn Persistence>,
        transcript: Arc<Transcript>,
        settings: Arc<SettingsHandle>,
        runners: Arc<RunnerMap>,
        tasks: Arc<BackgroundTasks>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            transcript,
            settings,
            runners,
            tasks,
            turn_targets: Mutex::new(HashMap::new()),
        })
    }

    /// Record that `turn_id` explicitly messaged or handed work off to `recipient`.
    pub fn record_turn_target(&self, turn_id: &TurnId, recipient: &BotId) {
        self.turn_targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(turn_id.clone())
            .or_default()
            .insert(recipient.clone());
    }

    /// Take and clear all recipient bot IDs recorded during `turn_id`.
    pub fn take_turn_targets(&self, turn_id: &TurnId) -> HashSet<BotId> {
        self.turn_targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(turn_id)
            .unwrap_or_default()
    }

    /// `message_bot {to, text, urgency}`.
    pub fn message_bot(
        &self,
        ctx: &SendContext,
        to: &str,
        text: &str,
        urgency: Urgency,
    ) -> Result<Sent, DomainError> {
        if let Some(blocked) = self.blocked(ctx)? {
            return Ok(blocked);
        }
        let recipient = self.resolve_bot(to)?;
        if recipient.id == ctx.from.id {
            return Err(DomainError::invalid(
                "that is you; there is nobody to message",
            ));
        }

        // Group work stays in the group.
        if let ConversationKind::Group { members, .. } = &ctx.conversation.kind
            && members.contains(&recipient.id)
        {
            let posted = self.post_in_group(ctx, &recipient, text, urgency)?;
            return Ok(posted);
        }

        let chain = self.next_chain(ctx)?;
        let entry = self
            .transcript
            .bot_message(
                &recipient.direct_conversation_id,
                &ctx.from.id,
                text,
                None,
                Some(ctx.from.id.clone()),
            )
            .map_err(|err| {
                warn!(error = %err, "cannot write into the recipient's DM");
                err
            })?;
        let _ = self.transcript.event(
            &ctx.conversation.id,
            "bot-messaged",
            format!("Messaged @{}", recipient.name),
        );

        let mut delivery = Delivery::bot_message(
            recipient.direct_conversation_id.clone(),
            ctx.from.id.clone(),
            recipient.id.clone(),
            text.to_string(),
            chain,
            urgency,
        );
        delivery.source_entry_id = Some(entry.id.clone());
        let woke = self.deliver(delivery, &recipient)?;
        Ok(Sent {
            conversation: recipient.direct_conversation_id,
            entry: Some(entry.id),
            woke,
            stopped: false,
            task: None,
        })
    }

    /// Forward a teammate's answer back to the bot that asked, once.
    ///
    /// This is deliberately **not** `message_bot`: the delivery is
    /// `DeliveryKind::BotReply`, and a turn triggered by one never forwards a
    /// reply back. An exchange is therefore "message, answer" and it stops
    /// there (D4.8); before this the answer was an ordinary `BotMessage` and
    /// two models could acknowledge each other until the hop limit. The asker
    /// is still woken — it has to read the answer to act on it — but the answer
    /// is the last thing that travels between them.
    pub fn forward_answer(
        &self,
        ctx: &SendContext,
        to: &BotId,
        text: &str,
    ) -> Result<Sent, DomainError> {
        if let Some(blocked) = self.blocked(ctx)? {
            return Ok(blocked);
        }
        let recipient = self.store.get_bot(to)?.ok_or_else(|| {
            DomainError::invalid(format!(
                "the teammate this answer belongs to ({to}) no longer exists"
            ))
        })?;
        let chain = self.next_chain(ctx)?;
        let entry = self.transcript.bot_message(
            &recipient.direct_conversation_id,
            &ctx.from.id,
            text,
            None,
            Some(ctx.from.id.clone()),
        )?;
        let mut delivery = Delivery::bot_reply(
            recipient.direct_conversation_id.clone(),
            ctx.from.id.clone(),
            recipient.id.clone(),
            text.to_string(),
            chain,
        );
        delivery.source_entry_id = Some(entry.id.clone());
        let woke = self.deliver(delivery, &recipient)?;
        Ok(Sent {
            conversation: recipient.direct_conversation_id,
            entry: Some(entry.id),
            woke,
            stopped: false,
            task: None,
        })
    }

    /// `post_to_group {group, text}` — speak in a group you belong to.
    pub fn post_to_group(
        &self,
        ctx: &SendContext,
        group: &str,
        text: &str,
    ) -> Result<Sent, DomainError> {
        self.post_to_group_recorded(ctx, group, text, None)
    }

    pub(crate) fn post_to_group_recorded(
        &self,
        ctx: &SendContext,
        group: &str,
        text: &str,
        turn_id: Option<&TurnId>,
    ) -> Result<Sent, DomainError> {
        if let Some(blocked) = self.blocked(ctx)? {
            return Ok(blocked);
        }
        let conversation = self.resolve_group(group, &ctx.from.id)?;
        let entry =
            self.transcript
                .bot_message(&conversation.id, &ctx.from.id, text, None, None)?;
        // Posting in a group can wake the members it mentions; the runner does
        // that for its own replies, and this does it for a direct post.
        let members = self.group_members(&conversation)?;
        let woken = crate::router::GroupRouter::wake_from_reply(
            &members,
            text,
            std::slice::from_ref(&ctx.from.id),
        );
        let hops = ctx.hops;
        let mut woke_any = false;
        let mut stopped = false;
        for bot in woken
            .iter()
            .filter_map(|id| members.iter().find(|m| &m.id == id))
        {
            let Some(chain) = self.chain_at(hops) else {
                let _ = self.hop_limit_event(&ctx.conversation.id, &ctx.from);
                stopped = true;
                break;
            };
            let mut delivery = Delivery::bot_message(
                conversation.id.clone(),
                ctx.from.id.clone(),
                bot.id.clone(),
                text.to_string(),
                chain,
                Urgency::Background,
            );
            delivery.source_entry_id = Some(entry.id.clone());
            match self.deliver(delivery, bot) {
                Ok(did_wake) => {
                    woke_any |= did_wake;
                    if did_wake && let Some(turn_id) = turn_id {
                        self.record_turn_target(turn_id, &bot.id);
                    }
                }
                Err(err) => {
                    warn!(error = %err, bot = %bot.name, "cannot wake group member after post_to_group");
                }
            }
        }
        Ok(Sent {
            conversation: conversation.id,
            entry: Some(entry.id),
            woke: woke_any,
            stopped,
            task: None,
        })
    }

    /// `handoff_task {to, task, context}` — give a teammate a piece of work and
    /// get their result back.
    pub fn handoff_task(
        &self,
        ctx: &SendContext,
        to: &str,
        title: &str,
        task_text: &str,
        context: Option<&str>,
    ) -> Result<Sent, DomainError> {
        if let Some(blocked) = self.blocked(ctx)? {
            return Ok(blocked);
        }
        let recipient = self.resolve_bot(to)?;
        if recipient.id == ctx.from.id {
            return Err(DomainError::invalid(
                "use start_task for your own work; a handoff needs somebody else",
            ));
        }
        let Some(chain) = self.chain_at(ctx.hops) else {
            self.hop_limit_event(&ctx.conversation.id, &ctx.from)?;
            return Ok(Sent {
                conversation: ctx.conversation.id.clone(),
                entry: None,
                woke: false,
                stopped: true,
                task: None,
            });
        };

        let mut task = Task::handoff(
            ctx.from.id.clone(),
            recipient.id.clone(),
            ctx.conversation.id.clone(),
            title,
            task_text,
            context.map(str::to_string),
        );
        // The chain travels with the task: the turn that runs it reads its hop
        // count from the delivery the pump makes, and without this a
        // handoff_task loop would restart the count at zero every hop.
        task.causal = chain;
        let entry = self.transcript.event(
            &ctx.conversation.id,
            "handoff",
            format!("Handed off \"{}\" to @{}", task.title, recipient.name),
        )?;

        // The pump decides where the work runs (the recipient's own DM) and
        // honours the per-bot cap; the result comes back to `ctx.conversation`.
        let task = self.tasks.submit(task)?;
        let woke = task.status == TaskStatus::Running;

        Ok(Sent {
            conversation: ctx.conversation.id.clone(),
            entry: Some(entry.id),
            woke,
            stopped: false,
            task: Some(task),
        })
    }

    /// `react {emoji}` — an acknowledgement that does not wake anybody.
    ///
    /// Recorded as an event line rather than as a reaction on the message: the
    /// transcript is append-only whole-entry upserts, and a one-line event is
    /// honest about what happened without inventing a reaction schema.
    pub fn react(&self, ctx: &SendContext, emoji: &str) -> Result<Sent, DomainError> {
        let target = self.last_other_message(ctx)?;
        let who = match &target {
            Some(entry) => match &entry.author {
                swarmbot_contracts::entry::Author::Owner => "the owner's message".to_string(),
                swarmbot_contracts::entry::Author::Bot { id } => self
                    .store
                    .get_bot(id)?
                    .map(|b| format!("@{}'s message", b.name))
                    .unwrap_or_else(|| "a message".to_string()),
                swarmbot_contracts::entry::Author::System => "a line".to_string(),
            },
            None => "nothing".to_string(),
        };
        self.transcript.event(
            &ctx.conversation.id,
            "react",
            format!("@{} reacted {emoji} to {who}", ctx.from.name),
        )?;
        Ok(Sent {
            conversation: ctx.conversation.id.clone(),
            entry: target.map(|e| e.id),
            woke: false,
            stopped: false,
            task: None,
        })
    }

    /// `list_bots` — who is on the team, and what they are for.
    pub fn list_bots(&self) -> Result<ToolResult, DomainError> {
        let bots = self.store.list_bots()?;
        if bots.is_empty() {
            return Ok(ToolResult::text("You are the only Bot."));
        }
        let mut lines = Vec::new();
        for bot in &bots {
            let status = bot.status.label();
            lines.push(format!(
                "- @{} ({}) — {} [{}]",
                bot.name,
                bot.label,
                first_line(&bot.instructions),
                status
            ));
        }
        Ok(ToolResult::text(lines.join("\n")))
    }

    /// Wake the teammates a reply mentioned. Called by the runner's hooks.
    pub fn wake_mentioned(
        &self,
        conversation: &Conversation,
        from: &Bot,
        text: &str,
        entry: Option<&Entry>,
        hops: u8,
        woken: Vec<BotId>,
    ) {
        let Some(chain) = self.chain_at(hops) else {
            if let Err(err) = self.hop_limit_event(&conversation.id, from) {
                debug!(error = %err, "cannot record the hop limit");
            }
            return;
        };
        for id in woken {
            let Ok(Some(bot)) = self.store.get_bot(&id) else {
                continue;
            };
            let delivery = Delivery::bot_message(
                conversation.id.clone(),
                from.id.clone(),
                bot.id.clone(),
                text.to_string(),
                chain.clone(),
                Urgency::Background,
            );
            let mut delivery = delivery;
            delivery.source_entry_id = entry.map(|e| e.id.clone());
            if let Err(err) = self.deliver(delivery, &bot) {
                warn!(error = %err, bot = %bot.name, "cannot wake a mentioned teammate");
            }
        }
    }

    // -- internals -----------------------------------------------------------

    /// Post into the group instead of a DM, and wake the recipient from there.
    fn post_in_group(
        &self,
        ctx: &SendContext,
        recipient: &Bot,
        text: &str,
        urgency: Urgency,
    ) -> Result<Sent, DomainError> {
        let already_mentions = crate::router::mentioned_handles(text)
            .iter()
            .any(|h| h.eq_ignore_ascii_case(&recipient.name));
        let posted = if already_mentions {
            text.to_string()
        } else {
            format!("@{} {}", recipient.name, text)
        };
        let entry = self.transcript.bot_message(
            &ctx.conversation.id,
            &ctx.from.id,
            &posted,
            None,
            Some(recipient.id.clone()),
        )?;
        let Some(chain) = self.chain_at(ctx.hops) else {
            self.hop_limit_event(&ctx.conversation.id, &ctx.from)?;
            return Ok(Sent {
                conversation: ctx.conversation.id.clone(),
                entry: Some(entry.id),
                woke: false,
                stopped: true,
                task: None,
            });
        };
        let mut delivery = Delivery::bot_message(
            ctx.conversation.id.clone(),
            ctx.from.id.clone(),
            recipient.id.clone(),
            text.to_string(),
            chain,
            urgency,
        );
        delivery.source_entry_id = Some(entry.id.clone());
        let woke = self.deliver(delivery, recipient)?;
        Ok(Sent {
            conversation: ctx.conversation.id.clone(),
            entry: Some(entry.id),
            woke,
            stopped: false,
            task: None,
        })
    }

    /// The kill switch and the hop limit, checked before anything is written.
    fn blocked(&self, ctx: &SendContext) -> Result<Option<Sent>, DomainError> {
        if !self.settings.get().bot_messaging {
            return Err(DomainError::conflict(
                "bot messaging is switched off by the owner",
            ));
        }
        if self.chain_at(ctx.hops).is_none() {
            self.hop_limit_event(&ctx.conversation.id, &ctx.from)?;
            return Ok(Some(Sent {
                conversation: ctx.conversation.id.clone(),
                entry: None,
                woke: false,
                stopped: true,
                task: None,
            }));
        }
        Ok(None)
    }

    fn next_chain(
        &self,
        ctx: &SendContext,
    ) -> Result<swarmbot_contracts::delivery::CausalChain, DomainError> {
        self.chain_at(ctx.hops)
            .ok_or_else(|| DomainError::conflict("too many hand-offs in this chain"))
    }

    /// The chain one hop on, or `None` at the limit.
    fn chain_at(&self, hops: u8) -> Option<swarmbot_contracts::delivery::CausalChain> {
        let limit = self.settings.get().max_hops;
        if hops >= limit {
            return None;
        }
        Some(swarmbot_contracts::delivery::CausalChain {
            root: swarmbot_contracts::id::DeliveryId::new(),
            hops: hops + 1,
        })
    }

    fn hop_limit_event(
        &self,
        conversation: &ConversationId,
        from: &Bot,
    ) -> Result<Entry, DomainError> {
        self.transcript.event(
            conversation,
            "hop-limit",
            format!(
                "Stopped: too many hand-offs (the limit is {}). @{} has the last word.",
                self.settings.get().max_hops,
                from.name
            ),
        )
    }

    fn deliver(&self, mut delivery: Delivery, recipient: &Bot) -> Result<bool, DomainError> {
        self.store.insert_delivery(&delivery)?;
        // A bot that is disabled is not woken; the delivery stays queued so the
        // owner can see what it missed.
        if matches!(
            recipient.status,
            swarmbot_contracts::bot::BotStatus::Disabled
        ) {
            delivery.mark_dropped(Utc::now());
            self.store.update_delivery(&delivery)?;
            return Ok(false);
        }
        self.runners.deliver(delivery)?;
        Ok(true)
    }

    /// Resolve a bot by handle, label or id.
    pub fn resolve_bot(&self, name: &str) -> Result<Bot, DomainError> {
        let handle = swarmbot_contracts::bot::normalize_name(name);
        if let Some(bot) = self.store.bot_by_name(&handle)? {
            return Ok(bot);
        }
        // A label or a raw id is accepted too: models often copy what they saw.
        let bots = self.store.list_bots()?;
        if let Some(bot) = bots
            .iter()
            .find(|b| b.label.eq_ignore_ascii_case(name.trim()) || b.id.as_str() == name.trim())
        {
            return Ok(bot.clone());
        }
        let known: Vec<String> = bots.iter().map(|b| format!("@{}", b.name)).collect();
        Err(DomainError::invalid(format!(
            "there is no bot called `{name}`; the team is {}",
            if known.is_empty() {
                "empty".to_string()
            } else {
                known.join(", ")
            }
        )))
    }

    /// Resolve a group by id or title, and check membership.
    pub fn resolve_group(&self, key: &str, member: &BotId) -> Result<Conversation, DomainError> {
        let key = key.trim();
        for conversation in self.store.list_conversations()? {
            let ConversationKind::Group { members, .. } = &conversation.kind else {
                continue;
            };
            let by_id = conversation.id.as_str() == key;
            let by_title = conversation
                .title
                .as_deref()
                .is_some_and(|title| title.eq_ignore_ascii_case(key));
            if (by_id || by_title) && members.contains(member) {
                return Ok(conversation);
            }
            if by_id || by_title {
                return Err(DomainError::Unauthorized {
                    message: format!("you are not a member of \"{key}\""),
                });
            }
        }
        Err(DomainError::invalid(format!(
            "there is no group called `{key}` that you are in"
        )))
    }

    pub fn group_members(&self, conversation: &Conversation) -> Result<Vec<Bot>, DomainError> {
        let ConversationKind::Group { members, .. } = &conversation.kind else {
            return Ok(Vec::new());
        };
        let mut bots = Vec::new();
        for id in members {
            if let Some(bot) = self.store.get_bot(id)? {
                bots.push(bot);
            }
        }
        Ok(bots)
    }

    /// The newest message in the conversation that is not the sender's own.
    fn last_other_message(&self, ctx: &SendContext) -> Result<Option<Entry>, DomainError> {
        let entries = self.transcript.tail(&ctx.conversation.id, 20)?;
        Ok(entries
            .into_iter()
            .rev()
            .find(|entry| entry.is_message() && entry.author.bot_id() != Some(&ctx.from.id)))
    }

    pub fn store(&self) -> Arc<dyn Persistence> {
        Arc::clone(&self.store)
    }

    pub fn transcript(&self) -> Arc<Transcript> {
        Arc::clone(&self.transcript)
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= 90 {
        line.to_string()
    } else {
        let cut: String = line.chars().take(90).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::BackgroundTasks;
    use crate::testing;

    struct Rig {
        messaging: Arc<Messaging>,
        store: Arc<dyn Persistence>,
        runners: Arc<RunnerMap>,
        lead: Bot,
        scout: Bot,
        group: Conversation,
    }

    fn rig() -> Rig {
        let fixture = testing::Fixture::memory();
        let lead = fixture.bot("lead");
        let scout = fixture.bot("scout");
        let _dev = fixture.bot("dev");
        // Not in the group: a handoff to it goes through the DM path.
        let _sentinel = fixture.bot("sentinel");
        let group = fixture.group("Team", &["lead", "scout", "dev"]);
        let settings = SettingsHandle::new(fixture.store.clone());
        let runners = RunnerMap::new();
        let tasks = BackgroundTasks::new(
            fixture.store.clone(),
            fixture.hub.clone(),
            fixture.transcript.clone(),
            settings.clone(),
            runners.clone(),
        );
        let messaging = Messaging::new(
            fixture.store.clone(),
            fixture.transcript.clone(),
            settings,
            runners.clone(),
            tasks,
        );
        Rig {
            messaging,
            store: fixture.store,
            runners,
            lead,
            scout,
            group,
        }
    }

    impl Rig {
        fn ctx_in_dm(&self) -> SendContext {
            SendContext {
                from: self.lead.clone(),
                conversation: self
                    .store
                    .get_conversation(&self.lead.direct_conversation_id)
                    .unwrap()
                    .unwrap(),
                hops: 0,
                source_entry: None,
            }
        }

        fn ctx_in_group(&self) -> SendContext {
            SendContext {
                from: self.lead.clone(),
                conversation: self.group.clone(),
                hops: 0,
                source_entry: None,
            }
        }
    }

    #[test]
    fn a_dm_message_lands_in_the_recipients_dm_and_wakes_them() {
        let rig = rig();
        let sent = rig
            .messaging
            .message_bot(
                &rig.ctx_in_dm(),
                "scout",
                "look at src/",
                Urgency::Background,
            )
            .unwrap();
        assert!(sent.woke);
        assert_eq!(sent.conversation, rig.scout.direct_conversation_id);

        let entries = rig
            .store
            .entry_tail(&rig.scout.direct_conversation_id, 10)
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].author.bot_id(), Some(&rig.lead.id));
        assert_eq!(entries[0].kind.text(), Some("look at src/"));

        // The sender's own DM shows the event line, not the message.
        let own = rig
            .store
            .entry_tail(&rig.lead.direct_conversation_id, 10)
            .unwrap();
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].kind.kind_str(), "event");
        assert!(own[0].kind.text().unwrap().contains("Messaged @scout"));

        // The delivery is parked for the runner that does not exist yet.
        assert_eq!(rig.runners.parked_count(), 1);
    }

    #[test]
    fn an_answer_is_forwarded_once_as_the_end_of_the_exchange() {
        let rig = rig();
        // Scout's turn is answering lead's message: it has already made one
        // hop, so the answer travels as hop 2.
        let ctx = SendContext {
            from: rig.scout.clone(),
            conversation: rig
                .store
                .get_conversation(&rig.scout.direct_conversation_id)
                .unwrap()
                .unwrap(),
            hops: 1,
            source_entry: None,
        };
        let sent = rig
            .messaging
            .forward_answer(&ctx, &rig.lead.id, "@lead Port is 8080.")
            .unwrap();
        assert!(sent.woke);
        assert_eq!(sent.conversation, rig.lead.direct_conversation_id);

        let entries = rig
            .store
            .entry_tail(&rig.lead.direct_conversation_id, 10)
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].author.bot_id(), Some(&rig.scout.id));
        assert_eq!(entries[0].kind.text(), Some("@lead Port is 8080."));

        let deliveries = rig.store.queued_deliveries(&rig.lead.id).unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(
            deliveries[0].kind,
            swarmbot_contracts::delivery::DeliveryKind::BotReply
        );
        assert!(deliveries[0].ends_exchange());
        assert_eq!(deliveries[0].from_bot, Some(rig.scout.id.clone()));
        assert_eq!(deliveries[0].causal.hops, 2);
        assert_eq!(deliveries[0].source_entry_id, Some(entries[0].id.clone()));
    }

    #[test]
    fn group_work_stays_in_the_group() {
        let rig = rig();
        let sent = rig
            .messaging
            .message_bot(
                &rig.ctx_in_group(),
                "scout",
                "check the diff",
                Urgency::Background,
            )
            .unwrap();
        assert_eq!(sent.conversation, rig.group.id, "posted in the group");

        let entries = rig.store.entry_tail(&rig.group.id, 10).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].kind.text().unwrap().starts_with("@scout "));

        // Nothing was written into scout's DM: no side channel.
        let dm = rig
            .store
            .entry_tail(&rig.scout.direct_conversation_id, 10)
            .unwrap();
        assert!(
            dm.is_empty(),
            "a fellow member is not DM'd from a group turn"
        );
    }

    #[test]
    fn messaging_a_stranger_is_an_error_that_names_the_team() {
        let rig = rig();
        let err = rig
            .messaging
            .message_bot(&rig.ctx_in_dm(), "nobody", "hi", Urgency::Background)
            .unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        assert!(err.to_string().contains("@scout"), "{err}");
    }

    #[test]
    fn messaging_yourself_is_refused() {
        let rig = rig();
        let err = rig
            .messaging
            .message_bot(&rig.ctx_in_dm(), "lead", "hi me", Urgency::Background)
            .unwrap_err();
        assert!(err.to_string().contains("that is you"), "{err}");
    }

    #[test]
    fn the_kill_switch_stops_everything() {
        let rig = rig();
        let settings = SettingsHandle::new(rig.store.clone());
        settings.update(|s| s.bot_messaging = false).unwrap();
        let err = rig
            .messaging
            .message_bot(&rig.ctx_in_dm(), "scout", "hi", Urgency::Background)
            .unwrap_err();
        assert_eq!(err.code(), "conflict");
        assert!(err.to_string().contains("switched off"));
    }

    #[test]
    fn the_hop_limit_stops_a_chain_and_says_so() {
        let rig = rig();
        let mut ctx = rig.ctx_in_dm();
        ctx.hops = 8;
        let sent = rig
            .messaging
            .message_bot(&ctx, "scout", "again", Urgency::Background)
            .unwrap();
        assert!(sent.stopped);
        assert!(!sent.woke);
        let entries = rig
            .store
            .entry_tail(&rig.lead.direct_conversation_id, 10)
            .unwrap();
        let line = entries.last().unwrap();
        assert_eq!(line.kind.kind_str(), "event");
        assert!(line.kind.text().unwrap().contains("too many hand-offs"));
    }

    #[test]
    fn a_handoff_creates_a_task_a_card_and_a_delivery() {
        let rig = rig();
        let sent = rig
            .messaging
            .handoff_task(
                &rig.ctx_in_group(),
                "sentinel",
                "review the diff",
                "look for risky changes",
                Some("the diff is in /tmp/x.patch"),
            )
            .unwrap();
        let task = sent.task.expect("a task was created");
        let sentinel = rig.store.bot_by_name("sentinel").unwrap().unwrap();
        assert_eq!(task.owner, sentinel.id);
        assert_eq!(task.requester, Some(rig.lead.id.clone()));
        assert_eq!(
            task.origin, rig.group.id,
            "the result comes back to the group"
        );
        // The pump placed it at once, so it is already on its way.
        assert_eq!(task.status, swarmbot_contracts::task::TaskStatus::Running);
        assert!(sent.woke);

        let stored = rig.store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(
            stored.context.as_deref(),
            Some("the diff is in /tmp/x.patch")
        );

        let entries = rig.store.entry_tail(&rig.group.id, 10).unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.kind.text().unwrap_or_default().contains("Handed off"))
        );

        // The work runs in the recipient's own DM, not in the group that asked:
        // a group is not a place for somebody else's tool calls.
        assert_eq!(rig.runners.parked_count(), 1);
        let deliveries = rig.store.queued_deliveries(&sentinel.id).unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(
            deliveries[0].conversation_id,
            sentinel.direct_conversation_id
        );
        assert_eq!(deliveries[0].task_id, Some(task.id.clone()));
        assert!(deliveries[0].text.contains("review the diff"));
        assert!(
            rig.store
                .queued_deliveries(&rig.lead.id)
                .unwrap()
                .is_empty(),
            "the requester does not get the work"
        );
    }

    #[test]
    fn a_handoff_carries_the_chain_that_created_it() {
        let rig = rig();
        let mut ctx = rig.ctx_in_group();
        ctx.hops = 3;
        let sent = rig
            .messaging
            .handoff_task(&ctx, "scout", "review the diff", "look for risk", None)
            .unwrap();
        let task = sent.task.expect("a task");
        assert_eq!(task.causal.hops, 4, "the chain travels with the task");

        // The pumped delivery carries it too, which is what the recipient's
        // turn reads when it decides whether the next hop is allowed.
        let scout = rig.store.bot_by_name("scout").unwrap().unwrap();
        let deliveries = rig.store.queued_deliveries(&scout.id).unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].causal.hops, 4);
    }

    #[test]
    fn a_handoff_chain_still_stops_at_the_hop_limit() {
        let rig = rig();
        let mut ctx = rig.ctx_in_group();
        ctx.hops = 8;
        let sent = rig
            .messaging
            .handoff_task(&ctx, "scout", "review", "check", None)
            .unwrap();
        assert!(sent.stopped);
        assert!(sent.task.is_none(), "no task is created past the limit");
        assert!(rig.store.queued_tasks().unwrap().is_empty());
        let entries = rig.store.entry_tail(&rig.group.id, 10).unwrap();
        assert!(entries.iter().any(|e| {
            e.kind
                .text()
                .unwrap_or_default()
                .contains("too many hand-offs")
        }));
    }

    #[test]
    fn a_handoff_to_yourself_points_at_start_task() {
        let rig = rig();
        let err = rig
            .messaging
            .handoff_task(&rig.ctx_in_dm(), "lead", "t", "b", None)
            .unwrap_err();
        assert!(err.to_string().contains("start_task"), "{err}");
    }

    #[test]
    fn posting_to_a_group_you_are_not_in_is_refused() {
        let rig = rig();
        let outsider = testing::seed_bot(&rig.store, "outsider");
        let ctx = SendContext {
            from: outsider.clone(),
            conversation: rig
                .store
                .get_conversation(&outsider.direct_conversation_id)
                .unwrap()
                .unwrap(),
            hops: 0,
            source_entry: None,
        };
        let err = rig
            .messaging
            .post_to_group(&ctx, "Team", "let me in")
            .unwrap_err();
        assert_eq!(err.code(), "unauthorized");
        assert!(err.to_string().contains("not a member"), "{err}");

        // A member addressing the group by title works.
        let sent = rig
            .messaging
            .post_to_group(&rig.ctx_in_dm(), "Team", "status?")
            .unwrap();
        assert_eq!(sent.conversation, rig.group.id);
    }

    #[test]
    fn a_group_post_wakes_the_members_it_mentions() {
        let rig = rig();
        let ctx = rig.ctx_in_dm();
        rig.messaging
            .post_to_group(&ctx, "Team", "@scout @dev look at this")
            .unwrap();
        assert_eq!(rig.runners.parked_count(), 2);
    }

    #[test]
    fn a_reaction_is_a_line_not_a_message() {
        let rig = rig();
        // There is a message from somebody else to react to.
        rig.messaging
            .message_bot(
                &rig.ctx_in_group(),
                "scout",
                "found it",
                Urgency::Background,
            )
            .unwrap();
        let ctx = SendContext {
            from: rig.scout.clone(),
            conversation: rig.group.clone(),
            hops: 1,
            source_entry: None,
        };
        let sent = rig.messaging.react(&ctx, "👍").unwrap();
        assert!(!sent.woke);
        let entries = rig.store.entry_tail(&rig.group.id, 10).unwrap();
        let last = entries.last().unwrap();
        assert_eq!(last.kind.kind_str(), "event");
        assert!(last.kind.text().unwrap().contains("reacted 👍"));
    }

    #[test]
    fn list_bots_describes_the_team() {
        let rig = rig();
        let result = rig.messaging.list_bots().unwrap();
        let text = result.text_only();
        assert!(text.contains("@lead"));
        assert!(text.contains("@scout"));
        assert!(text.contains("[idle]"));
    }

    #[test]
    fn a_disabled_bot_is_not_woken() {
        let rig = rig();
        let mut scout = rig.scout.clone();
        scout.status = swarmbot_contracts::bot::BotStatus::Disabled;
        rig.store.update_bot(&scout).unwrap();
        let sent = rig
            .messaging
            .message_bot(&rig.ctx_in_dm(), "scout", "you there?", Urgency::Urgent)
            .unwrap();
        assert!(!sent.woke, "a disabled bot stays asleep");
        assert_eq!(rig.runners.parked_count(), 0);
    }

    #[test]
    fn waking_mentioned_teammates_respects_the_hop_limit() {
        let rig = rig();
        let woken = vec![rig.scout.id.clone()];
        rig.messaging
            .wake_mentioned(&rig.group, &rig.lead, "@scout done", None, 8, woken);
        assert_eq!(rig.runners.parked_count(), 0);
        let entries = rig.store.entry_tail(&rig.group.id, 10).unwrap();
        assert!(entries.iter().any(|e| {
            e.kind
                .text()
                .unwrap_or_default()
                .contains("too many hand-offs")
        }));
    }

    #[test]
    fn bots_resolve_by_handle_label_or_id() {
        let rig = rig();
        assert_eq!(
            rig.messaging.resolve_bot("@Scout").unwrap().id,
            rig.scout.id
        );
        assert_eq!(rig.messaging.resolve_bot("scout").unwrap().id, rig.scout.id);
        assert_eq!(
            rig.messaging.resolve_bot(rig.scout.id.as_ref()).unwrap().id,
            rig.scout.id
        );
    }

    #[test]
    fn post_in_group_honors_urgency_sets_addressed_to_and_avoids_duplicate_mention() {
        let rig = rig();
        let sent = rig
            .messaging
            .message_bot(
                &rig.ctx_in_group(),
                "scout",
                "@scout please verify this",
                Urgency::Urgent,
            )
            .unwrap();
        assert_eq!(sent.conversation, rig.group.id);

        let entries = rig.store.entry_tail(&rig.group.id, 10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].kind.text(),
            Some("@scout please verify this"),
            "must not double-prefix @scout when already mentioned"
        );
        assert_eq!(
            entries[0]
                .as_message()
                .and_then(|m| m.addressed_to.as_ref()),
            Some(&rig.scout.id)
        );

        let deliveries = rig.store.queued_deliveries(&rig.scout.id).unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].urgency, Urgency::Urgent);
        assert_eq!(deliveries[0].source_entry_id, Some(entries[0].id.clone()));
    }

    #[test]
    fn post_to_group_reports_accurate_woke_and_sets_source_entry_id() {
        let rig = rig();
        // No mentions -> nobody woken -> woke == false.
        let unmentioned = rig
            .messaging
            .post_to_group(&rig.ctx_in_dm(), "Team", "just sharing status")
            .unwrap();
        assert!(!unmentioned.woke);
        assert!(unmentioned.entry.is_some());

        // Explicit mention -> teammate woken and delivery carries source_entry_id.
        let mentioned = rig
            .messaging
            .post_to_group(&rig.ctx_in_dm(), "Team", "@scout take a look")
            .unwrap();
        assert!(mentioned.woke);
        let deliveries = rig.store.queued_deliveries(&rig.scout.id).unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].source_entry_id, mentioned.entry);
    }
}

//! Conversations: groups, membership, history, and what deleting a group means.

use std::sync::Arc;

use chrono::Utc;
use tracing::info;

use swarmbot_contracts::Seq;
use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::conversation::{Conversation, ConversationKind};
use swarmbot_contracts::entry::Entry;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, ConversationId};
use swarmbot_contracts::repo::Persistence;

use crate::bus::{Change, LiveHub};
use crate::runners::RunnerMap;
use crate::tasks::BackgroundTasks;
use crate::transcript::Transcript;

/// How much history one page carries.
pub const HISTORY_PAGE: u32 = 100;

pub struct ConversationService {
    store: Arc<dyn Persistence>,
    hub: Arc<LiveHub>,
    transcript: Arc<Transcript>,
    runners: Arc<RunnerMap>,
    tasks: Arc<BackgroundTasks>,
}

impl ConversationService {
    pub fn new(
        store: Arc<dyn Persistence>,
        hub: Arc<LiveHub>,
        transcript: Arc<Transcript>,
        runners: Arc<RunnerMap>,
        tasks: Arc<BackgroundTasks>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            hub,
            transcript,
            runners,
            tasks,
        })
    }

    /// A group of 2–6 bots. The lead answers a message nobody was mentioned in.
    pub fn create_group(
        &self,
        title: &str,
        members: Vec<BotId>,
        lead: Option<BotId>,
    ) -> Result<Conversation, DomainError> {
        let mut resolved = Vec::new();
        for id in &members {
            if self.store.get_bot(id)?.is_none() {
                return Err(swarmbot_contracts::not_found::bot(id));
            }
            resolved.push(id.clone());
        }
        let lead = match lead {
            Some(id) => id,
            None => resolved
                .first()
                .cloned()
                .ok_or_else(|| DomainError::invalid("a group needs at least two bots"))?,
        };
        let conversation = Conversation::group(title, resolved, lead)?;
        self.store.insert_conversation(&conversation)?;
        self.hub.publish(Change::Conversation(conversation.clone()));
        let names = self.member_names(&conversation)?;
        let _ = self.transcript.event(
            &conversation.id,
            "group-created",
            format!("Group created with {}", names.join(", ")),
        );
        info!(group = %title, members = ?names, "group created");
        Ok(conversation)
    }

    pub fn list(&self) -> Result<Vec<Conversation>, DomainError> {
        Ok(self
            .store
            .list_conversations()?
            .into_iter()
            .filter(|c| !c.archived)
            .collect())
    }

    pub fn get(&self, id: &ConversationId) -> Result<Conversation, DomainError> {
        self.store
            .get_conversation(id)?
            .ok_or_else(|| swarmbot_contracts::not_found::conversation(id))
    }

    pub fn find_by_title(&self, title: &str) -> Result<Option<Conversation>, DomainError> {
        Ok(self
            .store
            .list_conversations()?
            .into_iter()
            .find(|c| c.title.as_deref() == Some(title)))
    }

    pub fn rename(&self, id: &ConversationId, title: &str) -> Result<Conversation, DomainError> {
        let mut conversation = self.get(id)?;
        if !conversation.kind.is_group() {
            return Err(DomainError::conflict(
                "a DM is named after its bot and cannot be renamed",
            ));
        }
        conversation.title = Some(title.to_string());
        conversation.updated_at = Utc::now();
        self.store.update_conversation(&conversation)?;
        self.hub.publish(Change::Conversation(conversation.clone()));
        Ok(conversation)
    }

    /// Update a group's title, members and/or lead.
    pub fn update_group(
        &self,
        id: &ConversationId,
        title: Option<String>,
        members: Option<Vec<BotId>>,
        lead: Option<BotId>,
    ) -> Result<Conversation, DomainError> {
        let mut conversation = self.get(id)?;
        let ConversationKind::Group {
            members: current_members,
            lead: current_lead,
        } = &conversation.kind
        else {
            return Err(DomainError::conflict("a DM cannot be updated as a group"));
        };

        if let Some(new_title) = title {
            let trimmed = new_title.trim();
            if trimmed.is_empty() {
                return Err(DomainError::invalid("a group needs a title"));
            }
            conversation.title = Some(trimmed.to_string());
        }

        let next_members = match members {
            Some(ids) => {
                if ids.is_empty() {
                    return Err(DomainError::invalid("a group needs at least two bots"));
                }
                for bot_id in &ids {
                    if self.store.get_bot(bot_id)?.is_none() {
                        return Err(swarmbot_contracts::not_found::bot(bot_id));
                    }
                }
                ids
            }
            None => current_members.clone(),
        };

        let next_lead = match lead {
            Some(bot_id) => {
                if self.store.get_bot(&bot_id)?.is_none() {
                    return Err(swarmbot_contracts::not_found::bot(&bot_id));
                }
                bot_id
            }
            None => current_lead.clone(),
        };

        let next_kind = ConversationKind::Group {
            members: next_members,
            lead: next_lead,
        };
        next_kind.validate()?;
        conversation.kind = next_kind;
        conversation.updated_at = Utc::now();
        self.store.update_conversation(&conversation)?;
        self.hub.publish(Change::Conversation(conversation.clone()));
        Ok(conversation)
    }

    /// History for a client: ascending by sequence, after a cursor.
    pub fn history(
        &self,
        id: &ConversationId,
        after_seq: Seq,
        limit: u32,
    ) -> Result<Vec<Entry>, DomainError> {
        self.get(id)?;
        self.store
            .entries_after(id, after_seq, if limit == 0 { HISTORY_PAGE } else { limit })
    }

    /// The newest entries, ascending — what a chat shows on open.
    pub fn tail(&self, id: &ConversationId, limit: u32) -> Result<Vec<Entry>, DomainError> {
        self.get(id)?;
        self.store
            .entry_tail(id, if limit == 0 { HISTORY_PAGE } else { limit })
    }

    pub fn members(&self, conversation: &Conversation) -> Result<Vec<Bot>, DomainError> {
        let mut bots = Vec::new();
        for id in conversation.kind.participants() {
            if let Some(bot) = self.store.get_bot(&id)? {
                bots.push(bot);
            }
        }
        Ok(bots)
    }

    pub fn member_names(&self, conversation: &Conversation) -> Result<Vec<String>, DomainError> {
        Ok(self
            .members(conversation)?
            .iter()
            .map(|bot| format!("@{}", bot.name))
            .collect())
    }

    /// The groups one bot belongs to.
    pub fn groups_of(&self, bot: &BotId) -> Result<Vec<Conversation>, DomainError> {
        Ok(self
            .store
            .list_conversations()?
            .into_iter()
            .filter(|c| c.kind.is_group() && c.kind.contains(bot))
            .collect())
    }

    /// Delete a group: its transcript, turns, cards, queued deliveries and
    /// unfinished tasks go with it. The member bots stay — what they saw in
    /// their own session is theirs, and a DM is never deleted here.
    pub fn delete_group(&self, id: &ConversationId) -> Result<(), DomainError> {
        let conversation = self.get(id)?;
        if !conversation.kind.is_group() {
            return Err(DomainError::conflict(
                "a DM cannot be deleted; disable or delete its bot instead",
            ));
        }

        // Stop the work before removing the record of it, or a turn would keep
        // writing into a conversation that no longer exists.
        for bot_id in conversation.kind.participants() {
            self.runners.interrupt(&bot_id, Some(id));
        }
        for mut request in self.store.requests_in(id)? {
            if request.is_pending() {
                request.cancel(Utc::now(), "the group was deleted");
                self.store.update_request(&request)?;
                self.hub.publish(Change::Request(request));
            }
        }
        for mut task in self.tasks.in_conversation(id)? {
            if !task.is_terminal() {
                task.cancel(Utc::now());
                self.store.update_task(&task)?;
                self.hub.publish(Change::Task(task));
            }
        }
        let _ = self.store.drop_deliveries(id, "the group was deleted");

        let removed = self.store.delete_entries(id)?;
        self.store.delete_conversation(id)?;
        self.transcript.forget(id);
        self.hub.publish(Change::ConversationDeleted(id.clone()));
        info!(conversation = %id, entries = removed, "group deleted");
        Ok(())
    }

    pub fn store(&self) -> Arc<dyn Persistence> {
        Arc::clone(&self.store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SettingsHandle;
    use crate::testing;
    use swarmbot_contracts::conversation::ConversationKind;
    use swarmbot_contracts::task::TaskStatus;

    struct Rig {
        service: Arc<ConversationService>,
        store: Arc<dyn Persistence>,
        lead: Bot,
        scout: Bot,
        dev: Bot,
    }

    fn rig() -> Rig {
        let fixture = testing::Fixture::memory();
        let lead = fixture.bot("lead");
        let scout = fixture.bot("scout");
        let dev = fixture.bot("dev");
        let runners = RunnerMap::new();
        let settings = SettingsHandle::new(fixture.store.clone());
        let tasks = BackgroundTasks::new(
            fixture.store.clone(),
            fixture.hub.clone(),
            fixture.transcript.clone(),
            settings,
            runners.clone(),
        );
        let service = ConversationService::new(
            fixture.store.clone(),
            fixture.hub.clone(),
            fixture.transcript.clone(),
            runners,
            tasks,
        );
        Rig {
            service,
            store: fixture.store,
            lead,
            scout,
            dev,
        }
    }

    #[test]
    fn a_group_is_created_with_its_members_and_lead() {
        let rig = rig();
        let group = rig
            .service
            .create_group(
                "Team",
                vec![rig.lead.id.clone(), rig.scout.id.clone()],
                Some(rig.lead.id.clone()),
            )
            .unwrap();
        assert!(group.kind.is_group());
        assert_eq!(
            rig.service.member_names(&group).unwrap(),
            vec!["@lead", "@scout"]
        );
        assert_eq!(
            rig.service.list().unwrap().len(),
            4,
            "three DMs and a group"
        );
    }

    #[test]
    fn the_first_member_leads_when_nobody_is_named() {
        let rig = rig();
        let group = rig
            .service
            .create_group("Team", vec![rig.scout.id.clone(), rig.dev.id.clone()], None)
            .unwrap();
        match &group.kind {
            ConversationKind::Group { lead, .. } => assert_eq!(*lead, rig.scout.id),
            _ => panic!("expected a group"),
        }
    }

    #[test]
    fn a_group_of_one_or_a_stranger_is_refused() {
        let rig = rig();
        let err = rig
            .service
            .create_group("Solo", vec![rig.lead.id.clone()], None)
            .unwrap_err();
        assert!(err.to_string().contains("2–6"), "{err}");

        let err = rig
            .service
            .create_group("Ghost", vec![rig.lead.id.clone(), BotId::new()], None)
            .unwrap_err();
        assert_eq!(err.code(), "not_found");
    }

    #[test]
    fn a_dm_cannot_be_renamed_or_deleted() {
        let rig = rig();
        let dm = rig.lead.direct_conversation_id.clone();
        assert_eq!(
            rig.service.rename(&dm, "Nope").unwrap_err().code(),
            "conflict"
        );
        assert_eq!(
            rig.service.delete_group(&dm).unwrap_err().code(),
            "conflict"
        );
    }

    #[test]
    fn renaming_publishes_the_new_title() {
        let rig = rig();
        let group = rig
            .service
            .create_group(
                "Team",
                vec![rig.lead.id.clone(), rig.scout.id.clone()],
                None,
            )
            .unwrap();
        let renamed = rig.service.rename(&group.id, "Crew").unwrap();
        assert_eq!(renamed.title.as_deref(), Some("Crew"));
        assert_eq!(
            rig.service.get(&group.id).unwrap().title.as_deref(),
            Some("Crew")
        );
    }

    #[test]
    fn history_pages_and_tails() {
        let rig = rig();
        let group = rig
            .service
            .create_group(
                "Team",
                vec![rig.lead.id.clone(), rig.scout.id.clone()],
                None,
            )
            .unwrap();
        for i in 0..5 {
            rig.service
                .transcript
                .owner_message(&group.id, format!("line {i}"))
                .unwrap();
        }
        // Creating the group wrote one event line, so the transcript holds six.
        assert_eq!(rig.service.history(&group.id, 0, 10).unwrap().len(), 6);
        assert_eq!(rig.service.history(&group.id, 3, 10).unwrap().len(), 3);
        let tail = rig.service.tail(&group.id, 2).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].seq, 5, "ascending, newest last");
    }

    #[test]
    fn deleting_a_group_takes_its_work_with_it() {
        let rig = rig();
        let group = rig
            .service
            .create_group(
                "Team",
                vec![rig.lead.id.clone(), rig.scout.id.clone()],
                None,
            )
            .unwrap();
        rig.service
            .transcript
            .owner_message(&group.id, "do the thing")
            .unwrap();
        let mut task = swarmbot_contracts::task::Task::own(
            rig.lead.id.clone(),
            group.id.clone(),
            "audit",
            "x",
        );
        rig.store.insert_task(&task).unwrap();
        let mut request = swarmbot_contracts::request::RuntimeRequest::question(
            rig.lead.id.clone(),
            group.id.clone(),
            "which?",
            vec![],
        );
        rig.store.insert_request(&request).unwrap();
        rig.store
            .insert_delivery(&swarmbot_contracts::delivery::Delivery::owner_message(
                group.id.clone(),
                rig.scout.id.clone(),
                "queued",
            ))
            .unwrap();

        rig.service.delete_group(&group.id).unwrap();

        assert!(rig.store.get_conversation(&group.id).unwrap().is_none());
        assert!(rig.store.entry_tail(&group.id, 10).unwrap().is_empty());
        task = rig.store.get_task(&task.id).unwrap().unwrap();
        assert_eq!(
            task.status,
            TaskStatus::Cancelled,
            "unfinished work is stopped"
        );
        request = rig.store.get_request(&request.id).unwrap().unwrap();
        assert_eq!(
            request.status,
            swarmbot_contracts::request::RequestStatus::Cancelled
        );
        assert!(
            rig.store
                .queued_deliveries(&rig.scout.id)
                .unwrap()
                .is_empty()
        );
        // The bots themselves stay, with their DMs.
        assert_eq!(rig.store.list_bots().unwrap().len(), 3);
        assert!(
            rig.store
                .get_conversation(&rig.lead.direct_conversation_id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn groups_of_a_bot_are_listed() {
        let rig = rig();
        rig.service
            .create_group(
                "Team",
                vec![rig.lead.id.clone(), rig.scout.id.clone()],
                None,
            )
            .unwrap();
        rig.service
            .create_group(
                "Other",
                vec![rig.scout.id.clone(), rig.dev.id.clone()],
                None,
            )
            .unwrap();
        assert_eq!(rig.service.groups_of(&rig.scout.id).unwrap().len(), 2);
        assert_eq!(rig.service.groups_of(&rig.lead.id).unwrap().len(), 1);
        assert_eq!(rig.service.groups_of(&rig.dev.id).unwrap().len(), 1);
        assert_eq!(
            rig.service
                .find_by_title("Team")
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("Team")
        );
        assert!(rig.service.find_by_title("Nope").unwrap().is_none());
    }

    #[test]
    fn update_group_updates_title_members_lead_and_validates() {
        let rig = rig();
        let group = rig
            .service
            .create_group(
                "Alpha",
                vec![rig.lead.id.clone(), rig.scout.id.clone()],
                Some(rig.lead.id.clone()),
            )
            .unwrap();
        let before = rig.service.hub.cursor();

        // Update title, expand members to include dev, and transfer lead to scout.
        let updated = rig
            .service
            .update_group(
                &group.id,
                Some("Beta".into()),
                Some(vec![
                    rig.lead.id.clone(),
                    rig.scout.id.clone(),
                    rig.dev.id.clone(),
                ]),
                Some(rig.scout.id.clone()),
            )
            .unwrap();
        assert_eq!(updated.title.as_deref(), Some("Beta"));
        assert_eq!(
            updated.kind,
            ConversationKind::Group {
                members: vec![
                    rig.lead.id.clone(),
                    rig.scout.id.clone(),
                    rig.dev.id.clone(),
                ],
                lead: rig.scout.id.clone(),
            }
        );
        let events = rig.store.events_since(before, 20).unwrap();
        assert!(events.iter().any(|(_, ev)| matches!(
            ev,
            swarmbot_contracts::LiveEvent::ConversationUpsert { conversation, .. }
                if conversation.id == group.id && conversation.title.as_deref() == Some("Beta")
        )));

        // Rejects DM, empty members, unknown bot, and lead outside members.
        assert!(
            rig.service
                .update_group(
                    &rig.lead.direct_conversation_id,
                    Some("Nope".into()),
                    None,
                    None
                )
                .is_err()
        );
        assert!(
            rig.service
                .update_group(&group.id, None, Some(vec![]), None)
                .is_err()
        );
        assert!(
            rig.service
                .update_group(
                    &group.id,
                    None,
                    Some(vec![rig.lead.id.clone(), BotId::new()]),
                    None
                )
                .is_err()
        );
        assert!(
            rig.service
                .update_group(
                    &group.id,
                    None,
                    Some(vec![rig.lead.id.clone(), rig.scout.id.clone()]),
                    Some(rig.dev.id.clone())
                )
                .is_err()
        );
    }
}

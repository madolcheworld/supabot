//! Bots: the roster, and the lifecycle around it.

use std::sync::Arc;

use chrono::Utc;
use tracing::info;

use swarmbot_contracts::bot::{Bot, BotStatus};
use swarmbot_contracts::config::{BotSpec, TeamSpec};
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, ConversationId, NodeId};
use swarmbot_contracts::repo::Persistence;
use swarmbot_contracts::tool::Toolset;

use crate::bus::{Change, LiveHub};
use crate::runners::RunnerMap;
use crate::session::Session;

/// What it takes to make a bot.
#[derive(Clone, Debug)]
pub struct NewBot {
    pub name: String,
    pub label: String,
    pub instructions: String,
    pub backend: String,
    pub model: Option<String>,
    pub color: Option<String>,
    pub node: Option<NodeId>,
    pub toolsets: Option<Vec<Toolset>>,
}

impl NewBot {
    pub fn simple(name: &str, label: &str, instructions: &str, backend: &str) -> Self {
        Self {
            name: name.to_string(),
            label: label.to_string(),
            instructions: instructions.to_string(),
            backend: backend.to_string(),
            model: None,
            color: None,
            node: None,
            toolsets: None,
        }
    }
}

pub struct BotService {
    store: Arc<dyn Persistence>,
    hub: Arc<LiveHub>,
    runners: Arc<RunnerMap>,
}

impl BotService {
    pub fn new(
        store: Arc<dyn Persistence>,
        hub: Arc<LiveHub>,
        runners: Arc<RunnerMap>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            hub,
            runners,
        })
    }

    /// Create a bot and its DM. Two names cannot collide: a handle is how bots
    /// address each other.
    pub fn create(&self, spec: NewBot) -> Result<Bot, DomainError> {
        let name = swarmbot_contracts::bot::normalize_name(&spec.name);
        if name.is_empty() {
            return Err(DomainError::invalid("a bot needs a name"));
        }
        if self.store.bot_by_name(&name)?.is_some() {
            return Err(DomainError::conflict(format!(
                "there is already a bot called @{name}"
            )));
        }
        let conversation = Conversation::direct(BotId::new());
        let mut bot = Bot::new(
            name,
            if spec.label.trim().is_empty() {
                &spec.name
            } else {
                &spec.label
            },
            spec.instructions,
            spec.backend,
            conversation.id.clone(),
        );
        // The DM belongs to this bot, so its kind has to name the real id.
        let conversation = Conversation {
            kind: swarmbot_contracts::conversation::ConversationKind::Direct {
                bot_id: bot.id.clone(),
            },
            ..conversation
        };
        bot.direct_conversation_id = conversation.id.clone();
        bot.model = spec.model;
        bot.color = spec.color;
        bot.node = spec.node;
        if let Some(toolsets) = spec.toolsets {
            bot.toolsets = toolsets;
        }

        self.store.insert_conversation(&conversation)?;
        self.store.insert_bot(&bot)?;
        self.hub.publish(Change::Conversation(conversation));
        self.hub.publish(Change::Bot(bot.clone()));
        info!(bot = %bot.name, backend = %bot.backend, "bot created");
        Ok(bot)
    }

    pub fn list(&self) -> Result<Vec<Bot>, DomainError> {
        self.store.list_bots()
    }

    pub fn get(&self, id: &BotId) -> Result<Bot, DomainError> {
        self.store
            .get_bot(id)?
            .ok_or_else(|| swarmbot_contracts::not_found::bot(id))
    }

    pub fn by_name(&self, name: &str) -> Result<Bot, DomainError> {
        let handle = swarmbot_contracts::bot::normalize_name(name);
        self.store
            .bot_by_name(&handle)?
            .ok_or_else(|| DomainError::invalid(format!("there is no bot called @{handle}")))
    }

    pub fn conversation_of(&self, id: &BotId) -> Result<Conversation, DomainError> {
        let bot = self.get(id)?;
        self.store
            .get_conversation(&bot.direct_conversation_id)?
            .ok_or_else(|| swarmbot_contracts::not_found::conversation(&bot.direct_conversation_id))
    }

    pub fn update(&self, bot: &Bot) -> Result<Bot, DomainError> {
        let mut stored = self.get(&bot.id)?;
        // The name is an address; changing it would break every mention in
        // flight, so it is not editable here.
        if stored.name != bot.name {
            return Err(DomainError::conflict(
                "a bot's handle cannot change while the team uses it",
            ));
        }
        stored.label = bot.label.clone();
        stored.instructions = bot.instructions.clone();
        stored.backend = bot.backend.clone();
        stored.model = bot.model.clone();
        stored.color = bot.color.clone();
        stored.node = bot.node.clone();
        stored.toolsets = bot.toolsets.clone();
        stored.skills = bot.skills.clone();
        stored.updated_at = Utc::now();
        self.store.update_bot(&stored)?;
        self.hub.publish(Change::Bot(stored.clone()));
        Ok(stored)
    }

    /// Turn a bot off (or on). A disabled bot is not woken by anything.
    pub fn set_enabled(&self, id: &BotId, enabled: bool) -> Result<Bot, DomainError> {
        let mut bot = self.get(id)?;
        bot.status = if enabled {
            BotStatus::Idle
        } else {
            BotStatus::Disabled
        };
        bot.updated_at = Utc::now();
        self.store.update_bot(&bot)?;
        self.hub.publish(Change::Bot(bot.clone()));
        if !enabled && let Some(handle) = self.runners.get(id) {
            handle.shutdown();
        }
        Ok(bot)
    }

    /// Remove a bot. Its transcript stays: deleting history would be a lie
    /// about what the team did.
    pub fn delete(&self, id: &BotId) -> Result<(), DomainError> {
        self.get(id)?;
        if let Some(handle) = self.runners.unregister(id) {
            handle.shutdown();
        }
        self.store.delete_session(id)?;
        self.store.delete_bot(id)?;
        Ok(())
    }

    /// "New session": the model forgets, the read cursors do not.
    pub fn new_session(&self, id: &BotId) -> Result<(), DomainError> {
        self.refuse_while_busy(id)?;
        let mut session = Session::load(self.store.as_ref(), id)?;
        session.reset_native();
        session.save(self.store.as_ref())?;
        Ok(())
    }

    /// "Compact": fold the old part of the history away. Returns how many
    /// messages were folded.
    pub fn compact(&self, id: &BotId) -> Result<usize, DomainError> {
        self.refuse_while_busy(id)?;
        let mut session = Session::load(self.store.as_ref(), id)?;
        let source = session.compaction_source();
        let folded = session.compact(crate::runners::compact_summary(&source));
        session.save(self.store.as_ref())?;
        Ok(folded)
    }

    fn refuse_while_busy(&self, id: &BotId) -> Result<(), DomainError> {
        if self.runners.is_busy(id) {
            return Err(DomainError::conflict(
                "the bot is working; stop it or wait for it to finish",
            ));
        }
        Ok(())
    }

    /// Create the roster a config describes: bots first, then their groups.
    /// Existing bots are kept (a restart must not duplicate a team).
    pub fn provision_team(
        &self,
        team: &TeamSpec,
        default_backend: &str,
        groups: &crate::conversations::ConversationService,
    ) -> Result<Vec<Bot>, DomainError> {
        team.validate()?;
        let mut bots = Vec::new();
        for spec in &team.bots {
            let bot = match self
                .store
                .bot_by_name(&swarmbot_contracts::bot::normalize_name(&spec.name))?
            {
                Some(existing) => existing,
                None => self.create(from_spec(spec, default_backend))?,
            };
            bots.push(bot);
        }
        for group in &team.groups {
            let members: Vec<BotId> = group
                .members
                .iter()
                .filter_map(|name| {
                    bots.iter()
                        .find(|b| b.name == swarmbot_contracts::bot::normalize_name(name))
                })
                .map(|b| b.id.clone())
                .collect();
            let lead = group
                .members
                .iter()
                .find(|name| **name == group.lead)
                .and_then(|name| {
                    bots.iter()
                        .find(|b| b.name == swarmbot_contracts::bot::normalize_name(name))
                })
                .map(|b| b.id.clone())
                .or_else(|| members.first().cloned());
            let (Some(lead), false) = (lead, members.len() < 2) else {
                continue;
            };
            if groups.find_by_title(&group.title)?.is_some() {
                continue;
            }
            groups.create_group(&group.title, members, Some(lead))?;
        }
        Ok(bots)
    }

    pub fn store(&self) -> Arc<dyn Persistence> {
        Arc::clone(&self.store)
    }
}

fn from_spec(spec: &BotSpec, default_backend: &str) -> NewBot {
    NewBot {
        name: spec.name.clone(),
        label: spec.label.clone(),
        instructions: spec.instructions.clone(),
        backend: spec
            .backend
            .clone()
            .unwrap_or_else(|| default_backend.to_string()),
        model: spec.model.clone(),
        color: spec.color.clone(),
        node: None,
        toolsets: spec.toolsets.clone(),
    }
}

/// The conversation id a bot's DM has, for callers that only have a bot.
pub fn direct_of(bot: &Bot) -> &ConversationId {
    &bot.direct_conversation_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversations::ConversationService;
    use crate::settings::SettingsHandle;
    use crate::testing;
    use crate::transcript::Transcript;
    use swarmbot_contracts::config::GroupSpec;

    struct Rig {
        service: Arc<BotService>,
        groups: Arc<ConversationService>,
        store: Arc<dyn Persistence>,
        runners: Arc<RunnerMap>,
        transcript: Arc<Transcript>,
    }

    fn rig() -> Rig {
        let fixture = testing::Fixture::memory();
        let runners = RunnerMap::new();
        let service = BotService::new(fixture.store.clone(), fixture.hub.clone(), runners.clone());
        let settings = SettingsHandle::new(fixture.store.clone());
        let tasks = crate::tasks::BackgroundTasks::new(
            fixture.store.clone(),
            fixture.hub.clone(),
            fixture.transcript.clone(),
            settings,
            runners.clone(),
        );
        let groups = ConversationService::new(
            fixture.store.clone(),
            fixture.hub.clone(),
            fixture.transcript.clone(),
            runners.clone(),
            tasks,
        );
        Rig {
            service,
            groups,
            store: fixture.store,
            runners,
            transcript: fixture.transcript,
        }
    }

    #[test]
    fn creating_a_bot_makes_its_dm() {
        let rig = rig();
        let bot = rig
            .service
            .create(NewBot::simple("lead", "Lead", "you lead", "fake"))
            .unwrap();
        assert_eq!(bot.name, "lead");
        let conversation = rig.service.conversation_of(&bot.id).unwrap();
        assert_eq!(conversation.direct_bot(), Some(&bot.id));
        assert_eq!(bot.direct_conversation_id, conversation.id);
        assert!(bot.toolsets.contains(&Toolset::Bot));
        assert!(
            rig.store
                .entry_tail(&conversation.id, 10)
                .unwrap()
                .is_empty(),
            "a new bot starts with an empty DM transcript"
        );
    }

    #[test]
    fn two_bots_cannot_share_a_handle() {
        let rig = rig();
        rig.service
            .create(NewBot::simple("lead", "Lead", "", "fake"))
            .unwrap();
        let err = rig
            .service
            .create(NewBot::simple("@Lead", "The Lead", "", "fake"))
            .unwrap_err();
        assert_eq!(err.code(), "conflict");
        assert!(err.to_string().contains("@lead"));
    }

    #[test]
    fn a_bot_without_a_name_is_refused() {
        let rig = rig();
        let err = rig
            .service
            .create(NewBot::simple("  ", "Nobody", "", "fake"))
            .unwrap_err();
        assert_eq!(err.code(), "invalid_input");
    }

    #[test]
    fn updating_keeps_the_handle_and_changes_the_rest() {
        let rig = rig();
        let bot = rig
            .service
            .create(NewBot::simple("scout", "Scout", "reads", "fake"))
            .unwrap();
        let mut edited = bot.clone();
        edited.label = "Scout Researcher".into();
        edited.instructions = "reads carefully".into();
        edited.model = Some("ali/qwen3.8-flash".into());
        let saved = rig.service.update(&edited).unwrap();
        assert_eq!(saved.label, "Scout Researcher");
        assert_eq!(saved.model.as_deref(), Some("ali/qwen3.8-flash"));

        let mut renamed = bot.clone();
        renamed.name = "researcher".into();
        let err = rig.service.update(&renamed).unwrap_err();
        assert_eq!(err.code(), "conflict");
    }

    #[test]
    fn disabling_stops_a_bot_being_woken() {
        let rig = rig();
        let bot = rig
            .service
            .create(NewBot::simple("dev", "Dev", "", "fake"))
            .unwrap();
        let off = rig.service.set_enabled(&bot.id, false).unwrap();
        assert_eq!(off.status, BotStatus::Disabled);
        let on = rig.service.set_enabled(&bot.id, true).unwrap();
        assert_eq!(on.status, BotStatus::Idle);
    }

    #[test]
    fn deleting_keeps_the_transcript() {
        let rig = rig();
        let bot = rig
            .service
            .create(NewBot::simple("dev", "Dev", "", "fake"))
            .unwrap();
        rig.transcript_note(&bot);
        rig.service.delete(&bot.id).unwrap();
        assert!(rig.service.get(&bot.id).is_err());
        let entries = rig
            .store
            .entry_tail(&bot.direct_conversation_id, 10)
            .unwrap();
        assert_eq!(entries.len(), 1, "what happened is still readable");
        assert_eq!(entries[0].kind.text(), Some("before the delete"));
        assert!(rig.runners.get(&bot.id).is_none());
    }

    impl Rig {
        fn transcript_note(&self, bot: &Bot) {
            // Through the transcript, not a raw upsert: it owns the sequence.
            self.transcript
                .owner_message(&bot.direct_conversation_id, "before the delete")
                .unwrap();
        }
    }

    #[test]
    fn a_new_session_keeps_the_read_cursors() {
        let rig = rig();
        let bot = rig
            .service
            .create(NewBot::simple("lead", "Lead", "", "fake"))
            .unwrap();
        let mut session = Session::load(rig.store.as_ref(), &bot.id).unwrap();
        session.set_native_session_id("native-1");
        session.push_user("old work");
        session.mark_seen(&bot.direct_conversation_id, 9);
        session.save(rig.store.as_ref()).unwrap();

        rig.service.new_session(&bot.id).unwrap();
        let after = Session::load(rig.store.as_ref(), &bot.id).unwrap();
        assert!(after.messages().is_empty());
        assert_eq!(after.native_session_id(), None);
        assert_eq!(after.seen(&bot.direct_conversation_id), 9);
    }

    #[test]
    fn provisioning_a_team_is_idempotent() {
        let rig = rig();
        let team = TeamSpec {
            bots: vec![
                BotSpec {
                    name: "lead".into(),
                    label: "Lead".into(),
                    instructions: "leads".into(),
                    backend: None,
                    model: None,
                    color: None,
                    node: None,
                    toolsets: None,
                    skills: vec![],
                },
                BotSpec {
                    name: "scout".into(),
                    label: "Scout".into(),
                    instructions: "reads".into(),
                    backend: None,
                    model: None,
                    color: None,
                    node: None,
                    toolsets: None,
                    skills: vec![],
                },
            ],
            groups: vec![GroupSpec {
                title: "Team".into(),
                members: vec!["lead".into(), "scout".into()],
                lead: "lead".into(),
            }],
        };
        let first = rig
            .service
            .provision_team(&team, "fake", &rig.groups)
            .unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].backend, "fake", "the default backend is used");
        let groups = |rig: &Rig| {
            rig.groups
                .list()
                .unwrap()
                .iter()
                .filter(|c| c.kind.is_group())
                .count()
        };
        assert_eq!(groups(&rig), 1);

        let second = rig
            .service
            .provision_team(&team, "fake", &rig.groups)
            .unwrap();
        assert_eq!(second[0].id, first[0].id, "a restart does not duplicate");
        assert_eq!(rig.service.list().unwrap().len(), 2);
        assert_eq!(groups(&rig), 1, "and the group is not created twice");
    }
}

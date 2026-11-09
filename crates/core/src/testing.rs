//! Fixtures and doubles: how tests (and the offline demo) get a working brain
//! without a database file, a model subscription or a network.
//!
//! Everything here is real code against the real ports — only the *store* is
//! in memory and the *backend* is scripted. That is the point: a test that
//! exercises `MemoryStore` and `FakeBackend` still runs the orchestration that
//! ships.

use std::path::Path;
use std::sync::Arc;

use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::config::{BackendSpec, Settings};
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, NodeId};
use swarmbot_contracts::node::{PolicyCeiling, PolicyMap, Surface};
use swarmbot_contracts::repo::Persistence;
use swarmbot_store::MemoryStore;

use crate::bus::LiveHub;
use crate::transcript::Transcript;

/// A store that lives in this process only.
pub fn memory_store() -> Arc<dyn Persistence> {
    Arc::new(MemoryStore::default())
}

/// A durable store under `dir` (`<dir>/swarmbot.sqlite`).
pub fn sqlite_store(dir: &Path) -> Result<Arc<dyn Persistence>, DomainError> {
    std::fs::create_dir_all(dir).map_err(|e| DomainError::internal(e.to_string()))?;
    let store = swarmbot_store::SqliteStore::open(&dir.join("swarmbot.sqlite"))?;
    Ok(Arc::new(store))
}

/// A transcript mirror under `<dir>/transcripts`.
pub fn mirror(dir: &Path) -> Arc<swarmbot_store::JsonlMirror> {
    Arc::new(swarmbot_store::JsonlMirror::new(dir.join("transcripts")))
}

/// A store, a hub and a transcript, wired the way the brain wires them.
pub struct Fixture {
    pub store: Arc<dyn Persistence>,
    pub hub: Arc<LiveHub>,
    pub transcript: Arc<Transcript>,
}

impl Fixture {
    pub fn memory() -> Self {
        Self::with_store(memory_store())
    }

    pub fn durable(dir: &Path) -> Result<Self, DomainError> {
        Ok(Self::with_store(sqlite_store(dir)?).with_mirror(dir))
    }

    fn with_store(store: Arc<dyn Persistence>) -> Self {
        let hub = LiveHub::new(Arc::clone(&store));
        let transcript = Transcript::new(Arc::clone(&store), Arc::clone(&hub), None);
        Self {
            store,
            hub,
            transcript,
        }
    }

    fn with_mirror(self, dir: &Path) -> Self {
        let transcript = Transcript::new(
            Arc::clone(&self.store),
            Arc::clone(&self.hub),
            Some(mirror(dir)),
        );
        Self { transcript, ..self }
    }

    /// A bot with its DM conversation, on the `fake` backend.
    pub fn bot(&self, name: &str) -> Bot {
        seed_bot(&self.store, name)
    }

    pub fn bot_on(&self, name: &str, backend: &str) -> Bot {
        seed_bot_on(&self.store, name, backend)
    }

    /// A group of existing bots, addressed by name. The first name leads.
    pub fn group(&self, title: &str, names: &[&str]) -> Conversation {
        seed_group(&self.store, title, names)
    }

    pub fn settings(&self) -> Settings {
        self.store.settings().unwrap_or_default()
    }
}

/// Create a bot and its DM conversation, returning the bot.
pub fn seed_bot(store: &Arc<dyn Persistence>, name: &str) -> Bot {
    seed_bot_on(store, name, "fake")
}

pub fn seed_bot_on(store: &Arc<dyn Persistence>, name: &str, backend: &str) -> Bot {
    let handle = swarmbot_contracts::bot::normalize_name(name);
    if let Ok(Some(existing)) = store.bot_by_name(&handle) {
        return existing;
    }
    let conversation = Conversation::direct(BotId::new());
    // The DM belongs to the bot, so it has to exist first: create the bot with
    // a placeholder conversation id, then fix both rows.
    let mut bot = Bot::new(
        handle.clone(),
        display_name(name),
        persona_for(&handle),
        backend,
        conversation.id.clone(),
    );
    let conversation = Conversation {
        kind: swarmbot_contracts::conversation::ConversationKind::Direct {
            bot_id: bot.id.clone(),
        },
        ..conversation
    };
    bot.direct_conversation_id = conversation.id.clone();
    store
        .insert_conversation(&conversation)
        .expect("conversation");
    store.insert_bot(&bot).expect("bot");
    bot
}

/// A group of bots that must already exist, by name.
pub fn seed_group(store: &Arc<dyn Persistence>, title: &str, names: &[&str]) -> Conversation {
    let members: Vec<BotId> = names
        .iter()
        .map(|name| {
            store
                .bot_by_name(&swarmbot_contracts::bot::normalize_name(name))
                .expect("lookup")
                .unwrap_or_else(|| panic!("no bot named {name}"))
                .id
        })
        .collect();
    let lead = members
        .first()
        .cloned()
        .unwrap_or_else(|| panic!("a group needs at least one member"));
    let conversation = Conversation::group(title, members, lead).expect("valid group");
    store
        .insert_conversation(&conversation)
        .expect("conversation");
    conversation
}

fn display_name(name: &str) -> String {
    let trimmed = name.trim().trim_start_matches('@');
    let mut out = String::new();
    let mut upper = true;
    for ch in trimmed.chars() {
        if ch == '-' || ch == '_' || ch == ' ' {
            upper = true;
            out.push(' ');
        } else if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

/// A one-line persona per demo role, so a fixture team behaves like a team.
fn persona_for(handle: &str) -> String {
    match handle {
        "lead" => "You lead the team: break work up, hand it out, and write the final answer.",
        "scout" => "You explore the workspace and report what you find, with paths and lines.",
        "dev" => "You write and change files, and you run commands to check your work.",
        "sentinel" => "You review the work and say plainly whether it is safe to keep.",
        _ => "You are a teammate. Do your part and report back.",
    }
    .to_string()
}

/// The local node every fixture brain has.
pub fn local_node() -> swarmbot_contracts::node::NodeInfo {
    let mut policy = PolicyMap::default();
    policy.set(Surface::ReadFile, PolicyCeiling::Always);
    policy.set(Surface::Shell, PolicyCeiling::Always);
    policy.set(Surface::WriteFile, PolicyCeiling::Always);
    swarmbot_contracts::node::NodeInfo::local(
        "this machine",
        vec![swarmbot_contracts::node::NodeRoot::read_write(
            "work",
            std::env::temp_dir()
                .join("swarmbot-fixture")
                .to_string_lossy()
                .to_string(),
        )],
        policy,
    )
}

/// A `fake` backend spec, for configs built in tests.
pub fn fake_backend_spec() -> BackendSpec {
    BackendSpec::fake("fake")
}

/// A node id that is not the local one.
pub fn remote_node_id() -> NodeId {
    NodeId::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_a_bot_creates_its_dm() {
        let store = memory_store();
        let bot = seed_bot(&store, "@Scout Researcher");
        assert_eq!(bot.name, "scout-researcher");
        assert_eq!(bot.label, "Scout Researcher");
        let conversation = store
            .get_conversation(&bot.direct_conversation_id)
            .unwrap()
            .expect("the DM exists");
        assert_eq!(conversation.direct_bot(), Some(&bot.id));
        // Seeding is idempotent: the same name gives the same bot back.
        let again = seed_bot(&store, "scout-researcher");
        assert_eq!(again.id, bot.id);
    }

    #[test]
    fn a_group_holds_its_members_and_lead() {
        let store = memory_store();
        seed_bot(&store, "lead");
        seed_bot(&store, "scout");
        let group = seed_group(&store, "Team", &["lead", "scout"]);
        let stored = store.get_conversation(&group.id).unwrap().unwrap();
        assert!(stored.kind.is_group());
        assert_eq!(stored.kind.participants().len(), 2);
    }

    #[test]
    fn display_names_are_title_cased() {
        assert_eq!(display_name("lead"), "Lead");
        assert_eq!(display_name("dev-ops"), "Dev Ops");
        assert_eq!(display_name("@scout_researcher"), "Scout Researcher");
    }

    #[test]
    fn the_fixture_writes_through_to_the_hub() {
        let fixture = Fixture::memory();
        let bot = fixture.bot("lead");
        fixture
            .transcript
            .owner_message(&bot.direct_conversation_id, "hello")
            .unwrap();
        let snapshot = fixture.hub.snapshot(None).unwrap();
        assert_eq!(snapshot.bots.len(), 1);
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(fixture.settings().max_hops, 8);
    }

    #[test]
    fn a_durable_fixture_survives_being_reopened() {
        let dir = tempfile::tempdir().unwrap();
        {
            let fixture = Fixture::durable(dir.path()).unwrap();
            let bot = fixture.bot("lead");
            fixture
                .transcript
                .owner_message(&bot.direct_conversation_id, "remember me")
                .unwrap();
        }
        let reopened = Fixture::durable(dir.path()).unwrap();
        let bots = reopened.store.list_bots().unwrap();
        assert_eq!(bots.len(), 1);
        let entries = reopened
            .store
            .entry_tail(&bots[0].direct_conversation_id, 10)
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind.text(), Some("remember me"));
        // The JSONL mirror can rebuild what the database lost.
        let mirror_entries = reopened
            .store
            .entry_tail(&bots[0].direct_conversation_id, 10)
            .unwrap();
        assert_eq!(mirror_entries.len(), 1);
    }
}

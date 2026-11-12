//! Target resolution: one string from the command line, one conversation to
//! talk in.
//!
//! `<TARGET>` may be a bot handle (`lead`, `@Lead`, `bot:lead`), a group title
//! (`Team`, `group:Team`, `#Team`) or a raw conversation id (`cnv_…`). Bare
//! targets are tried in exact order: bot handle, then group title, then
//! conversation id — a handle is what people type, an id is what scripts paste.
//! Explicit prefixes (`@` / `bot:`, `#` / `group:`) disambiguate when a bot and
//! a group share a name. The caller is told *which* interpretation matched,
//! because silently guessing a DM when the user meant a group is the kind of
//! surprise a CLI must not have.

use anyhow::{Result, anyhow};
use swarmbot_contracts::bot::{Bot, normalize_name};
use swarmbot_contracts::conversation::Conversation;
use swarmbot_contracts::id::ConversationId;
use swarmbot_core::swarm::Swarm;

/// What a `<TARGET>` turned out to be.
#[derive(Clone, Debug)]
pub enum Target {
    /// A bot handle: the conversation is its DM with the owner.
    Bot { bot: Bot },
    /// A group title or a group's conversation id.
    Group { conversation: Conversation },
}

impl Target {
    pub fn conversation_id(&self) -> &ConversationId {
        match self {
            Self::Bot { bot } => &bot.direct_conversation_id,
            Self::Group { conversation } => &conversation.id,
        }
    }

    /// The line that says which interpretation matched.
    pub fn describe(&self) -> String {
        match self {
            Self::Bot { bot } => format!(
                "bot @{} (dm {})",
                bot.name,
                short(bot.direct_conversation_id.as_str())
            ),
            Self::Group { conversation } => format!(
                "group \"{}\" ({})",
                conversation.title.as_deref().unwrap_or("untitled"),
                short(conversation.id.as_str())
            ),
        }
    }

    /// The bot whose DM this is, when the target is one.
    pub fn bot(&self) -> Option<&Bot> {
        match self {
            Self::Bot { bot } => Some(bot),
            Self::Group { .. } => None,
        }
    }
}

/// The roster a target is resolved against. A plain snapshot of two lists, so
/// resolution is testable without a swarm.
#[derive(Clone, Debug, Default)]
pub struct Directory {
    bots: Vec<Bot>,
    conversations: Vec<Conversation>,
}

impl Directory {
    pub fn new(bots: Vec<Bot>, conversations: Vec<Conversation>) -> Self {
        Self {
            bots,
            conversations,
        }
    }

    pub fn load(swarm: &Swarm) -> Result<Self> {
        Ok(Self::new(swarm.bots.list()?, swarm.conversations.list()?))
    }

    pub fn groups(&self) -> Vec<&Conversation> {
        self.conversations
            .iter()
            .filter(|c| c.kind.is_group())
            .collect()
    }

    /// Resolve a group explicitly by title (case-insensitive) or group id.
    pub fn resolve_group(&self, raw: &str) -> Result<Conversation> {
        let key = raw.trim();
        if key.is_empty() {
            return Err(anyhow!("a group name cannot be empty"));
        }
        if let Some(conversation) = self.conversations.iter().find(|c| {
            c.kind.is_group()
                && c.title
                    .as_deref()
                    .is_some_and(|title| title.eq_ignore_ascii_case(key))
        }) {
            return Ok(conversation.clone());
        }
        if let Ok(id) = ConversationId::parse(key)
            && let Some(conversation) = self
                .conversations
                .iter()
                .find(|c| c.kind.is_group() && c.id == id)
        {
            return Ok(conversation.clone());
        }
        let groups: Vec<String> = self
            .groups()
            .iter()
            .map(|c| format!("\"{}\"", c.title.as_deref().unwrap_or("untitled")))
            .collect();
        Err(anyhow!(
            "there is no group called \"{key}\" (groups: {})",
            if groups.is_empty() {
                "(none)".into()
            } else {
                groups.join(", ")
            }
        ))
    }

    /// Resolve a bot explicitly by handle (`lead`, `@lead`, case-insensitive).
    pub fn resolve_bot(&self, raw: &str) -> Result<Bot> {
        let handle = normalize_name(raw.trim());
        if handle.is_empty() {
            return Err(anyhow!("a bot handle cannot be empty"));
        }
        self.bots
            .iter()
            .find(|b| b.name == handle)
            .cloned()
            .ok_or_else(|| {
                let bots: Vec<String> = self.bots.iter().map(|b| format!("@{}", b.name)).collect();
                anyhow!(
                    "there is no bot called @{handle} (bots: {})",
                    if bots.is_empty() {
                        "(none)".into()
                    } else {
                        bots.join(", ")
                    }
                )
            })
    }

    /// When a bare target resolved to a bot whose handle matches a group title,
    /// tell the caller how to reach the group instead of leaving them guessing.
    pub fn ambiguity_hint(&self, raw: &str, matched: &Target) -> Option<String> {
        let key = raw.trim();
        if key.starts_with('@')
            || key.starts_with('#')
            || strip_prefix_ignore_ascii_case(key, "bot:").is_some()
            || strip_prefix_ignore_ascii_case(key, "group:").is_some()
        {
            return None;
        }
        if let Target::Bot { bot } = matched
            && let Some(group) = self.conversations.iter().find(|c| {
                c.kind.is_group()
                    && c.title
                        .as_deref()
                        .is_some_and(|title| title.eq_ignore_ascii_case(&bot.name))
            })
        {
            let title = group.title.as_deref().unwrap_or(&bot.name);
            return Some(format!(
                "both bot @{} and group \"{title}\" exist — matched bot; use `#{title}` or `group:{title}` for the group",
                bot.name
            ));
        }
        None
    }

    /// Bot handle, then group title, then conversation id — the documented
    /// order. Explicit prefixes (`group:` / `#` and `bot:` / `@`) pick one side
    /// directly. An unknown target is an error that lists what does exist,
    /// because "not found" alone sends people guessing.
    pub fn resolve(&self, raw: &str) -> Result<Target> {
        let key = raw.trim();
        if key.is_empty() {
            return Err(anyhow!("a target cannot be empty"));
        }

        if let Some(group_key) =
            strip_prefix_ignore_ascii_case(key, "group:").or_else(|| key.strip_prefix('#'))
        {
            let conversation = self.resolve_group(group_key)?;
            return Ok(Target::Group { conversation });
        }
        if let Some(bot_key) =
            strip_prefix_ignore_ascii_case(key, "bot:").or_else(|| key.strip_prefix('@'))
        {
            let bot = self.resolve_bot(bot_key)?;
            return Ok(Target::Bot { bot });
        }

        // 1. A bot handle: case-insensitive (normalize_name does the same
        //    folding a mention would).
        let handle = normalize_name(key);
        if !handle.is_empty()
            && let Some(bot) = self.bots.iter().find(|b| b.name == handle)
        {
            return Ok(Target::Bot { bot: bot.clone() });
        }

        // 2. A group title, case-insensitively (titles are human prose).
        if let Some(conversation) = self.conversations.iter().find(|c| {
            c.kind.is_group()
                && c.title
                    .as_deref()
                    .is_some_and(|title| title.eq_ignore_ascii_case(key))
        }) {
            return Ok(Target::Group {
                conversation: conversation.clone(),
            });
        }

        // 3. A raw conversation id — a group's or a bot DM's.
        if let Ok(id) = ConversationId::parse(key)
            && let Some(conversation) = self.conversations.iter().find(|c| c.id == id)
        {
            return match conversation.direct_bot() {
                Some(bot_id) => match self.bots.iter().find(|b| &b.id == bot_id) {
                    Some(bot) => Ok(Target::Bot { bot: bot.clone() }),
                    // The DM outlived its bot (deleting a bot keeps the
                    // transcript): fall through to the honest error below.
                    None => Err(self.unknown(key)),
                },
                None => Ok(Target::Group {
                    conversation: conversation.clone(),
                }),
            };
        }

        Err(self.unknown(key))
    }

    fn unknown(&self, key: &str) -> anyhow::Error {
        let bots: Vec<String> = self.bots.iter().map(|b| format!("@{}", b.name)).collect();
        let groups: Vec<String> = self
            .groups()
            .iter()
            .map(|c| format!("\"{}\"", c.title.as_deref().unwrap_or("untitled")))
            .collect();
        anyhow!(
            "there is no bot, group or conversation called `{key}`\n  bots: {}\n  groups: {}",
            if bots.is_empty() {
                "(none)".into()
            } else {
                bots.join(", ")
            },
            if groups.is_empty() {
                "(none)".into()
            } else {
                groups.join(", ")
            },
        )
    }
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Ids are long; the tail is the distinguishing part in a one-line message.
fn short(id: &str) -> String {
    let tail: String = id
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarmbot_contracts::id::BotId;

    /// A directory shaped like the demo team: two bots and one group.
    fn directory() -> (Directory, Bot, Bot, Conversation) {
        let lead_dm = ConversationId::new();
        let scout_dm = ConversationId::new();
        let lead = Bot::new("lead", "Lead", "leads", "fake", lead_dm.clone());
        let scout = Bot::new("scout", "Scout", "reads", "fake", scout_dm.clone());
        let group = Conversation::group(
            "Team Room",
            vec![lead.id.clone(), scout.id.clone()],
            lead.id.clone(),
        )
        .expect("a valid group");
        let mut conversations = vec![
            Conversation {
                kind: swarmbot_contracts::conversation::ConversationKind::Direct {
                    bot_id: lead.id.clone(),
                },
                ..Conversation::direct(BotId::new())
            },
            Conversation {
                kind: swarmbot_contracts::conversation::ConversationKind::Direct {
                    bot_id: scout.id.clone(),
                },
                ..Conversation::direct(BotId::new())
            },
            group.clone(),
        ];
        // The DMs must carry their real ids for id-resolution to work.
        conversations[0].id = lead.direct_conversation_id.clone();
        conversations[1].id = scout.direct_conversation_id.clone();
        let directory = Directory::new(vec![lead.clone(), scout.clone()], conversations);
        (directory, lead, scout, group)
    }

    #[test]
    fn a_handle_resolves_with_or_without_the_at_sign() {
        let (directory, lead, _, _) = directory();
        for key in ["lead", "@lead", "@Lead", "  LEAD "] {
            match directory.resolve(key).expect("resolves") {
                Target::Bot { bot } => assert_eq!(bot.id, lead.id, "{key}"),
                other => panic!("{key} resolved to {other:?}"),
            }
        }
    }

    #[test]
    fn a_group_title_resolves_case_insensitively() {
        let (directory, _, _, group) = directory();
        match directory.resolve("team room").expect("resolves") {
            Target::Group { conversation } => assert_eq!(conversation.id, group.id),
            other => panic!("resolved to {other:?}"),
        }
        match directory.resolve("TEAM ROOM").expect("resolves") {
            Target::Group { conversation } => assert_eq!(conversation.id, group.id),
            other => panic!("resolved to {other:?}"),
        }
    }

    #[test]
    fn raw_conversation_ids_resolve_to_what_they_are() {
        let (directory, lead, _, group) = directory();
        match directory.resolve(group.id.as_str()).expect("resolves") {
            Target::Group { conversation } => assert_eq!(conversation.id, group.id),
            other => panic!("resolved to {other:?}"),
        }
        match directory
            .resolve(lead.direct_conversation_id.as_str())
            .expect("resolves")
        {
            Target::Bot { bot } => assert_eq!(bot.id, lead.id, "a DM id means its bot"),
            other => panic!("resolved to {other:?}"),
        }
    }

    #[test]
    fn the_order_is_handle_then_title_then_id() {
        // A bot literally named like the group title: the handle wins.
        let dm = ConversationId::new();
        let team = Bot::new("team", "Team", "", "fake", dm.clone());
        let other_dm = ConversationId::new();
        let mate = Bot::new("mate", "Mate", "", "fake", other_dm.clone());
        let group = Conversation::group(
            "Team",
            vec![team.id.clone(), mate.id.clone()],
            team.id.clone(),
        )
        .expect("a valid group");
        let directory = Directory::new(
            vec![team.clone(), mate],
            vec![
                Conversation {
                    kind: swarmbot_contracts::conversation::ConversationKind::Direct {
                        bot_id: team.id.clone(),
                    },
                    id: dm,
                    ..Conversation::direct(BotId::new())
                },
                group,
            ],
        );
        assert!(matches!(
            directory.resolve("team").expect("resolves"),
            Target::Bot { .. }
        ));
    }

    #[test]
    fn an_unknown_target_lists_what_exists() {
        let (directory, _, _, _) = directory();
        let err = directory.resolve("nobody").expect_err("unknown");
        let text = err.to_string();
        assert!(text.contains("nobody"), "{text}");
        assert!(text.contains("@lead") && text.contains("@scout"), "{text}");
        assert!(text.contains("\"Team Room\""), "{text}");
    }

    #[test]
    fn an_empty_target_is_refused() {
        let (directory, _, _, _) = directory();
        assert!(directory.resolve("   ").is_err());
    }

    #[test]
    fn describe_names_the_match() {
        let (directory, _, _, group) = directory();
        let target = directory.resolve("lead").expect("resolves");
        assert!(
            target.describe().starts_with("bot @lead (dm …"),
            "{}",
            target.describe()
        );
        let target = directory.resolve("Team Room").expect("resolves");
        assert_eq!(target.conversation_id(), &group.id);
        assert!(
            target.describe().contains("group \"Team Room\""),
            "{}",
            target.describe()
        );
        assert!(target.bot().is_none());
    }

    #[test]
    fn explicit_prefixes_disambiguate_shared_bot_and_group_names() {
        let dm = ConversationId::new();
        let team_bot = Bot::new("team", "Team", "", "fake", dm.clone());
        let scout = Bot::new("scout", "Scout", "", "fake", ConversationId::new());
        let group = Conversation::group(
            "Team",
            vec![team_bot.id.clone(), scout.id.clone()],
            team_bot.id.clone(),
        )
        .expect("a valid group");
        let directory = Directory::new(
            vec![team_bot.clone(), scout],
            vec![
                Conversation {
                    kind: swarmbot_contracts::conversation::ConversationKind::Direct {
                        bot_id: team_bot.id.clone(),
                    },
                    id: dm,
                    ..Conversation::direct(BotId::new())
                },
                group.clone(),
            ],
        );

        let bare = directory.resolve("team").expect("resolves bare");
        assert!(matches!(bare, Target::Bot { .. }));
        let hint = directory
            .ambiguity_hint("team", &bare)
            .expect("warns about ambiguity");
        assert!(
            hint.contains("#Team") && hint.contains("group:Team"),
            "{hint}"
        );

        for expr in ["group:team", "#Team", "#team"] {
            let resolved = directory.resolve(expr).expect("resolves group prefix");
            assert_eq!(resolved.conversation_id(), &group.id, "{expr}");
            assert!(matches!(resolved, Target::Group { .. }), "{expr}");
            assert!(directory.ambiguity_hint(expr, &resolved).is_none());
        }
        for expr in ["bot:team", "@team"] {
            let resolved = directory.resolve(expr).expect("resolves bot prefix");
            assert_eq!(resolved.bot().expect("bot").id, team_bot.id, "{expr}");
            assert!(directory.ambiguity_hint(expr, &resolved).is_none());
        }
        assert!(
            directory.resolve_group("scout").is_err(),
            "resolve_group ignores bot handles"
        );
    }
}

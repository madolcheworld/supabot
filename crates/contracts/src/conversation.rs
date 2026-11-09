//! Conversations: a DM with one bot, or a group of 2–6 bots.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Seq;
use crate::id::{BotId, ConversationId};

/// The most bots one group may have (Grok Bot's rule; keeps routing sane).
pub const MAX_GROUP_MEMBERS: usize = 6;
pub const MIN_GROUP_MEMBERS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversationKind {
    /// The owner's 1:1 chat with one bot. Every bot has exactly one.
    Direct { bot_id: BotId },
    /// A group of bots. `lead` answers a message nobody was mentioned in.
    Group { members: Vec<BotId>, lead: BotId },
}

impl ConversationKind {
    pub fn is_group(&self) -> bool {
        matches!(self, Self::Group { .. })
    }

    /// Every bot that takes part in this conversation.
    pub fn participants(&self) -> Vec<BotId> {
        match self {
            Self::Direct { bot_id } => vec![bot_id.clone()],
            Self::Group { members, .. } => members.clone(),
        }
    }

    pub fn contains(&self, bot: &BotId) -> bool {
        self.participants().iter().any(|b| b == bot)
    }

    /// Validate a group: 2–6 members, no duplicates, and the lead is a member.
    pub fn validate(&self) -> Result<(), crate::error::DomainError> {
        match self {
            Self::Direct { .. } => Ok(()),
            Self::Group { members, lead } => {
                if members.len() < MIN_GROUP_MEMBERS || members.len() > MAX_GROUP_MEMBERS {
                    return Err(crate::error::DomainError::invalid(format!(
                        "a group has {MIN_GROUP_MEMBERS}–{MAX_GROUP_MEMBERS} bots, got {}",
                        members.len()
                    )));
                }
                let mut seen = std::collections::BTreeSet::new();
                for member in members {
                    if !seen.insert(member) {
                        return Err(crate::error::DomainError::invalid(format!(
                            "duplicate group member {member}"
                        )));
                    }
                }
                if !members.contains(lead) {
                    return Err(crate::error::DomainError::invalid(format!(
                        "lead {lead} is not a member of the group"
                    )));
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    /// Owner-editable title (groups); `None` for a DM, which renders as the
    /// bot's name.
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Highest sequence handed out in this conversation.
    pub last_seq: Seq,
    pub archived: bool,
}

impl Conversation {
    pub fn direct(bot_id: BotId) -> Self {
        let now = Utc::now();
        Self {
            id: ConversationId::new(),
            kind: ConversationKind::Direct { bot_id },
            title: None,
            created_at: now,
            updated_at: now,
            last_seq: 0,
            archived: false,
        }
    }

    pub fn group(
        title: impl Into<String>,
        members: Vec<BotId>,
        lead: BotId,
    ) -> Result<Self, crate::error::DomainError> {
        let kind = ConversationKind::Group { members, lead };
        kind.validate()?;
        let now = Utc::now();
        Ok(Self {
            id: ConversationId::new(),
            kind,
            title: Some(title.into()),
            created_at: now,
            updated_at: now,
            last_seq: 0,
            archived: false,
        })
    }

    pub fn is_direct(&self) -> bool {
        !self.kind.is_group()
    }

    /// The bot a DM belongs to.
    pub fn direct_bot(&self) -> Option<&BotId> {
        match &self.kind {
            ConversationKind::Direct { bot_id } => Some(bot_id),
            ConversationKind::Group { .. } => None,
        }
    }

    pub fn next_seq(&mut self) -> Seq {
        self.last_seq += 1;
        self.last_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bots(n: usize) -> Vec<BotId> {
        (0..n).map(|_| BotId::new()).collect()
    }

    #[test]
    fn direct_conversations_hold_one_bot() {
        let bot = BotId::new();
        let c = Conversation::direct(bot.clone());
        assert!(c.is_direct());
        assert_eq!(c.direct_bot(), Some(&bot));
        assert!(c.kind.contains(&bot));
        assert_eq!(c.kind.participants(), vec![bot]);
    }

    #[test]
    fn groups_need_two_to_six_members_and_a_member_lead() {
        let b = bots(2);
        assert!(Conversation::group("pair", b.clone(), b[0].clone()).is_ok());

        let one = bots(1);
        let err = Conversation::group("solo", one.clone(), one[0].clone()).unwrap_err();
        assert!(err.to_string().contains("2–6"), "{err}");

        let seven = bots(7);
        assert!(Conversation::group("crowd", seven.clone(), seven[0].clone()).is_err());

        let three = bots(3);
        let outsider = BotId::new();
        let err = Conversation::group("bad lead", three, outsider).unwrap_err();
        assert!(err.to_string().contains("not a member"), "{err}");
    }

    #[test]
    fn duplicate_members_are_rejected() {
        let bot = BotId::new();
        let err = Conversation::group("twins", vec![bot.clone(), bot.clone()], bot).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn sequences_count_up() {
        let mut c = Conversation::direct(BotId::new());
        assert_eq!(c.next_seq(), 1);
        assert_eq!(c.next_seq(), 2);
        assert_eq!(c.last_seq, 2);
    }
}

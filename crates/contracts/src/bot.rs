//! Bots: the named AI teammates.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::{BotId, ConversationId, NodeId};
use crate::tool::Toolset;

/// What a bot is doing right now (live roster state, not persisted history).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BotStatus {
    #[default]
    Idle,
    /// Running a turn; `activity` is the one-line status the UI shows.
    Working {
        conversation_id: ConversationId,
        activity: String,
    },
    /// Deliveries are waiting.
    Queued { count: u32 },
    /// The backend failed repeatedly; the reason is shown to the owner.
    Error { reason: String },
    /// Turned off by the owner: it is not woken by anything.
    Disabled,
}

impl BotStatus {
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Working { .. })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working { .. } => "working",
            Self::Queued { .. } => "queued",
            Self::Error { .. } => "error",
            Self::Disabled => "disabled",
        }
    }
}

/// A bot's stored definition.
///
/// A bot has **one mind**: one LLM session shared by every conversation it
/// takes part in, so what it did in a group is remembered in its DM.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bot {
    pub id: BotId,
    /// Lowercase handle used in `@mentions` and by `message_bot`.
    pub name: String,
    /// Display name.
    pub label: String,
    /// The persona: what this bot is for, how it works, what it owns.
    pub instructions: String,
    /// Which configured LLM backend runs this bot.
    pub backend: String,
    /// Model override; `None` uses the backend's default.
    pub model: Option<String>,
    /// `#rrggbb`, used for the avatar tile.
    pub color: Option<String>,
    /// The node this bot's tools run on; `None` = the local node.
    pub node: Option<NodeId>,
    /// Tool sets this bot may use, before placement and capability gating.
    pub toolsets: Vec<Toolset>,
    /// Skill names this bot has (a library comes later).
    pub skills: Vec<String>,
    /// Its 1:1 conversation with the owner.
    pub direct_conversation_id: ConversationId,
    pub status: BotStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Bot {
    /// A bot with the defaults a roster entry usually wants: bot + node tools,
    /// no skills, idle.
    pub fn new(
        name: impl Into<String>,
        label: impl Into<String>,
        instructions: impl Into<String>,
        backend: impl Into<String>,
        direct_conversation_id: ConversationId,
    ) -> Self {
        let now = Utc::now();
        let name = name.into();
        Self {
            id: BotId::new(),
            name: normalize_name(&name),
            label: label.into(),
            instructions: instructions.into(),
            backend: backend.into(),
            model: None,
            color: None,
            node: None,
            toolsets: vec![Toolset::Bot, Toolset::Node, Toolset::Task],
            skills: Vec::new(),
            direct_conversation_id,
            status: BotStatus::default(),
            created_at: now,
            updated_at: now,
        }
    }

    /// The mention token other bots and the owner type.
    pub fn mention(&self) -> String {
        format!("@{}", self.name)
    }

    /// A deterministic colour when the roster does not give one: FNV-1a over
    /// the id, mapped into a readable hue range.
    pub fn effective_color(&self) -> String {
        if let Some(color) = &self.color {
            return color.clone();
        }
        let hash = fnv1a(self.id.as_str().as_bytes());
        let hue = hash % 360;
        format!("#{:06x}", hsl_to_rgb(hue as f32, 0.55, 0.55))
    }

    /// Two hex colours are "the same bot" to the UI; keep this cheap.
    pub fn host_node(&self) -> NodeId {
        self.node.clone().unwrap_or_else(NodeId::local)
    }
}

/// Mentions and tool arguments are lowercase handles.
pub fn normalize_name(name: &str) -> String {
    name.trim()
        .trim_start_matches('@')
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> u32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| (((v + m).clamp(0.0, 1.0) * 255.0).round() as u32) & 0xff;
    (to(r1) << 16) | (to(g1) << 8) | to(b1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_normalized_handles() {
        assert_eq!(normalize_name("@Scout Researcher"), "scout-researcher");
        assert_eq!(normalize_name("  Lead  "), "lead");
        assert_eq!(normalize_name("Dev_Ops"), "dev_ops");
        let bot = Bot::new(
            "@Scout",
            "Scout Researcher",
            "reads the workspace",
            "fake",
            ConversationId::new(),
        );
        assert_eq!(bot.name, "scout");
        assert_eq!(bot.mention(), "@scout");
    }

    #[test]
    fn default_tools_cover_delegation_and_files() {
        let bot = Bot::new("lead", "Lead", "", "fake", ConversationId::new());
        assert!(bot.toolsets.contains(&Toolset::Bot));
        assert!(bot.toolsets.contains(&Toolset::Node));
        assert!(bot.toolsets.contains(&Toolset::Task));
        assert!(!bot.toolsets.contains(&Toolset::Owner));
    }

    #[test]
    fn host_node_defaults_to_local() {
        let mut bot = Bot::new("lead", "Lead", "", "fake", ConversationId::new());
        assert!(bot.host_node().is_local());
        let node = NodeId::new();
        bot.node = Some(node.clone());
        assert_eq!(bot.host_node(), node);
    }

    #[test]
    fn colors_are_deterministic_and_valid() {
        let bot = Bot::new("lead", "Lead", "", "fake", ConversationId::new());
        let a = bot.effective_color();
        let b = bot.clone().effective_color();
        assert_eq!(a, b);
        assert_eq!(a.len(), 7);
        assert!(a.starts_with('#'));
        assert!(u32::from_str_radix(&a[1..], 16).is_ok());

        let mut painted = bot;
        painted.color = Some("#123456".into());
        assert_eq!(painted.effective_color(), "#123456");
    }

    #[test]
    fn status_labels_are_stable() {
        assert_eq!(BotStatus::default().label(), "idle");
        assert!(
            BotStatus::Working {
                conversation_id: ConversationId::new(),
                activity: "Thinking".into()
            }
            .is_busy()
        );
        assert!(!BotStatus::Queued { count: 2 }.is_busy());
        let json = serde_json::to_value(BotStatus::Error { reason: "x".into() }).unwrap();
        assert_eq!(json["state"], "error");
    }
}

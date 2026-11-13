//! Runtime requests: approvals, questions, forms, secrets and hand-offs.
//!
//! One shape for all of them, one RPC to answer all of them, and one rule that
//! keeps a tool call from ever blocking on a human: the async asks
//! (`ask_user`, `request_approval`) return `{status: pending, id}` at once and
//! the answer arrives later as a delivery.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{BotId, ConversationId, EntryId, RequestId, RuleId, TaskId, TurnId};
use crate::node::Surface;

/// Unattended requests expire; a card then offers "always allow next time".
pub const APPROVAL_EXPIRY_SECS: u64 = 10 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// May this action run?
    Approval,
    /// A question with free-form or choice answers.
    Question,
    /// Several fields at once.
    Form,
    /// A credential the bot needs; never echoed into the transcript.
    Secret,
    /// The owner has to do something on a screen (login, 2FA, CAPTCHA).
    Handoff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Pending,
    Answered,
    Expired,
    Cancelled,
}

impl RequestStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// What the owner (or a rule) decided about an approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    AllowOnce,
    /// Also writes an [`ApprovalRule`], so the same ask never comes again.
    AllowAlways,
    Deny,
}

impl Decision {
    pub fn allows(self) -> bool {
        !matches!(self, Self::Deny)
    }
}

/// Who settled a request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Responder {
    Owner,
    /// An allow/deny rule answered it before any human saw it.
    Rule {
        rule_id: RuleId,
    },
    /// Nobody answered in time.
    Expiry,
    /// The turn was stopped or the conversation deleted.
    System {
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Multiline,
    Secret,
    Boolean,
    Choice,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    pub name: String,
    pub label: String,
    pub kind: FieldKind,
    #[serde(default)]
    pub choices: Vec<String>,
    #[serde(default)]
    pub required: bool,
    pub default: Option<String>,
}

impl FormField {
    pub fn text(name: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            kind: FieldKind::Text,
            choices: Vec::new(),
            required: false,
            default: None,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuntimeRequest {
    pub id: RequestId,
    pub bot_id: BotId,
    /// The conversation of the turn that raised it — cards sit next to the
    /// work, including in a group.
    pub conversation_id: ConversationId,
    pub turn_id: Option<TurnId>,
    pub task_id: Option<TaskId>,
    pub kind: RequestKind,
    /// The surface a rule matches on (`shell`, `write_file`, …).
    pub surface: Option<Surface>,
    pub tool: Option<String>,
    /// What the action acts on: a command, a path, a URL.
    pub target: Option<String>,
    /// Why, in the bot's own words.
    pub reason: Option<String>,
    /// The question text, for `Question`.
    pub prompt: Option<String>,
    #[serde(default)]
    pub fields: Vec<FormField>,
    pub status: RequestStatus,
    pub decision: Option<Decision>,
    pub answer: Option<Value>,
    pub responded_by: Option<Responder>,
    pub card_entry_id: Option<EntryId>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub resolved_at: Option<DateTime<Utc>>,
}

impl RuntimeRequest {
    fn base(bot_id: BotId, conversation_id: ConversationId, kind: RequestKind) -> Self {
        Self {
            id: RequestId::new(),
            bot_id,
            conversation_id,
            turn_id: None,
            task_id: None,
            kind,
            surface: None,
            tool: None,
            target: None,
            reason: None,
            prompt: None,
            fields: Vec::new(),
            status: RequestStatus::Pending,
            decision: None,
            answer: None,
            responded_by: None,
            card_entry_id: None,
            created_at: Utc::now(),
            expires_at: None,
            resolved_at: None,
        }
    }

    /// An approval for one action on one surface.
    pub fn approval(
        bot_id: BotId,
        conversation_id: ConversationId,
        surface: Surface,
        tool: impl Into<String>,
        target: impl Into<String>,
        reason: Option<String>,
    ) -> Self {
        let mut req = Self::base(bot_id, conversation_id, RequestKind::Approval);
        req.surface = Some(surface);
        req.tool = Some(tool.into());
        req.target = Some(target.into());
        req.reason = reason;
        req.expires_at =
            Some(req.created_at + chrono::Duration::seconds(APPROVAL_EXPIRY_SECS as i64));
        req
    }

    /// A question the bot asks the owner; never blocks a tool call.
    pub fn question(
        bot_id: BotId,
        conversation_id: ConversationId,
        prompt: impl Into<String>,
        fields: Vec<FormField>,
    ) -> Self {
        let mut req = Self::base(bot_id, conversation_id, RequestKind::Question);
        req.prompt = Some(prompt.into());
        req.fields = fields;
        req.expires_at =
            Some(req.created_at + chrono::Duration::seconds(APPROVAL_EXPIRY_SECS as i64));
        req
    }

    pub fn is_pending(&self) -> bool {
        matches!(self.status, RequestStatus::Pending)
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.is_pending() && self.expires_at.is_some_and(|at| at <= now)
    }

    /// Record the answer. Returns false when the request was already settled.
    pub fn respond(
        &mut self,
        now: DateTime<Utc>,
        decision: Option<Decision>,
        answer: Option<Value>,
        by: Responder,
    ) -> bool {
        if !self.is_pending() {
            return false;
        }
        self.status = RequestStatus::Answered;
        self.decision = decision;
        self.answer = answer;
        self.responded_by = Some(by);
        self.resolved_at = Some(now);
        true
    }

    pub fn expire(&mut self, now: DateTime<Utc>) -> bool {
        if !self.is_pending() {
            return false;
        }
        self.status = RequestStatus::Expired;
        self.responded_by = Some(Responder::Expiry);
        self.resolved_at = Some(now);
        true
    }

    pub fn cancel(&mut self, now: DateTime<Utc>, reason: impl Into<String>) -> bool {
        if !self.is_pending() {
            return false;
        }
        self.status = RequestStatus::Cancelled;
        self.responded_by = Some(Responder::System {
            reason: reason.into(),
        });
        self.resolved_at = Some(now);
        true
    }

    /// One line for a card title.
    pub fn title(&self) -> String {
        match self.kind {
            RequestKind::Approval => match (&self.surface, &self.target) {
                (Some(surface), Some(target)) => format!("Allow {surface} `{target}`?"),
                (Some(surface), None) => format!("Allow {surface}?"),
                _ => "Allow this action?".into(),
            },
            RequestKind::Question => self
                .prompt
                .clone()
                .unwrap_or_else(|| "A bot has a question".into()),
            RequestKind::Form => "Fill in this form".into(),
            RequestKind::Secret => "A bot needs a credential".into(),
            RequestKind::Handoff => "A bot needs you at the screen".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleEffect {
    Allow,
    Ask,
    Deny,
}

/// Externally tagged on purpose: a rule reads `{"scope": "global"}` or
/// `{"scope": {"bot": "bot_…"}}`, not a tag nested inside its own field name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleScope {
    Global,
    Bot(BotId),
}

/// A saved answer: "always allow `shell` starting with `cargo`", "always ask
/// for `write_file`", "never `rm -rf`".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRule {
    pub id: RuleId,
    pub scope: RuleScope,
    pub surface: Option<Surface>,
    pub tool: Option<String>,
    /// Matches the start of the action's target (a command prefix, a path).
    pub command_prefix: Option<String>,
    pub effect: RuleEffect,
    pub created_at: DateTime<Utc>,
}

impl ApprovalRule {
    pub fn new(scope: RuleScope, effect: RuleEffect) -> Self {
        Self {
            id: RuleId::new(),
            scope,
            surface: None,
            tool: None,
            command_prefix: None,
            effect,
            created_at: Utc::now(),
        }
    }

    /// Does this rule speak to that request?
    pub fn matches(&self, request: &RuntimeRequest) -> bool {
        match (&self.scope, &request.bot_id) {
            (RuleScope::Global, _) => {}
            (RuleScope::Bot(bot_id), requester) if bot_id == requester => {}
            (RuleScope::Bot(_), _) => return false,
        }
        if let Some(surface) = self.surface
            && request.surface != Some(surface)
        {
            return false;
        }
        if let Some(tool) = &self.tool
            && request.tool.as_deref() != Some(tool)
        {
            return false;
        }
        if let Some(prefix) = &self.command_prefix {
            let target = request.target.as_deref().unwrap_or_default();
            if !target_matches_prefix(target, prefix) {
                return false;
            }
        }
        true
    }
}

/// True when `target` starts with `prefix` **on a word boundary**.
///
/// A bare `starts_with` was a security hole, not a convenience: `command_prefix`
/// holds the first word of a command, so `ls` matched `lsof`, and one
/// "always allow `rm build/x`" — which stores the prefix `rm` — authorised
/// `rm -rf /`. The boundary is what keeps a saved prefix as narrow as the
/// answer that created it.
fn target_matches_prefix(target: &str, prefix: &str) -> bool {
    let Some(rest) = target.strip_prefix(prefix) else {
        return false;
    };
    rest.is_empty() || rest.starts_with(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> RuntimeRequest {
        RuntimeRequest::approval(
            BotId::new(),
            ConversationId::new(),
            Surface::Shell,
            "shell",
            "cargo test",
            Some("run the tests".into()),
        )
    }

    #[test]
    fn approvals_expire_after_ten_minutes() {
        let req = request();
        assert!(req.is_pending());
        assert_eq!(req.title(), "Allow shell `cargo test`?");
        assert!(!req.is_expired(req.created_at));
        let later = req.created_at + chrono::Duration::seconds(APPROVAL_EXPIRY_SECS as i64 + 1);
        assert!(req.is_expired(later));
    }

    #[test]
    fn answering_twice_does_not_overwrite() {
        let mut req = request();
        assert!(req.respond(
            Utc::now(),
            Some(Decision::AllowOnce),
            None,
            Responder::Owner
        ));
        assert_eq!(req.status, RequestStatus::Answered);
        assert!(!req.respond(Utc::now(), Some(Decision::Deny), None, Responder::Owner));
        assert_eq!(req.decision, Some(Decision::AllowOnce));
    }

    #[test]
    fn expiry_and_cancel_are_terminal() {
        let mut req = request();
        assert!(req.expire(Utc::now()));
        assert_eq!(req.status, RequestStatus::Expired);
        assert!(!req.expire(Utc::now()));

        let mut req2 = request();
        assert!(req2.cancel(Utc::now(), "turn stopped"));
        assert_eq!(req2.status, RequestStatus::Cancelled);
        assert!(req2.responded_by.is_some());
    }

    #[test]
    fn rules_match_on_scope_surface_tool_and_prefix() {
        let req = request();
        let mut rule = ApprovalRule::new(RuleScope::Global, RuleEffect::Allow);
        rule.surface = Some(Surface::Shell);
        rule.command_prefix = Some("cargo".into());
        assert!(rule.matches(&req));

        rule.command_prefix = Some("rm".into());
        assert!(!rule.matches(&req));

        rule.command_prefix = None;
        rule.surface = Some(Surface::WriteFile);
        assert!(!rule.matches(&req));

        rule.surface = Some(Surface::Shell);
        rule.scope = RuleScope::Bot(BotId::new());
        assert!(!rule.matches(&req), "another bot's rule must not match");

        rule.scope = RuleScope::Bot(req.bot_id.clone());
        assert!(rule.matches(&req));
    }

    #[test]
    fn a_command_prefix_matches_on_a_word_boundary_only() {
        // The bug this pins: `command_prefix` stores the first word of a
        // command, so a bare `starts_with` let one "always allow `rm build/x`"
        // (prefix `rm`) authorise `rm -rf /`, and `ls` matched `lsof`.
        let mut rule = ApprovalRule::new(RuleScope::Global, RuleEffect::Allow);
        rule.surface = Some(Surface::Shell);
        rule.command_prefix = Some("ls".into());

        let ls = RuntimeRequest::approval(
            BotId::new(),
            ConversationId::new(),
            Surface::Shell,
            "shell",
            "ls -la",
            None,
        );
        assert!(rule.matches(&ls), "an exact first word still matches");

        let lsof = RuntimeRequest::approval(
            BotId::new(),
            ConversationId::new(),
            Surface::Shell,
            "shell",
            "lsof -i :8787",
            None,
        );
        assert!(
            !rule.matches(&lsof),
            "`ls` must not match `lsof`: no word boundary, no match"
        );

        rule.command_prefix = Some("rm".into());
        let rm_rf = RuntimeRequest::approval(
            BotId::new(),
            ConversationId::new(),
            Surface::Shell,
            "shell",
            "rm -rf /",
            None,
        );
        // The prefix still matches the *text*; refusing this is the
        // approval service's job (`evaluate` downgrades a destructive Allow to
        // Ask). What must hold here is the narrower contract: the boundary.
        assert!(rule.matches(&rm_rf));
        assert!(
            !rule.matches(&RuntimeRequest::approval(
                BotId::new(),
                ConversationId::new(),
                Surface::Shell,
                "shell",
                "rmdir /tmp/x",
                None,
            )),
            "`rm` must not match `rmdir`"
        );
    }

    #[test]
    fn serde_round_trips_every_kind() {
        for kind in [
            RequestKind::Approval,
            RequestKind::Question,
            RequestKind::Form,
            RequestKind::Secret,
            RequestKind::Handoff,
        ] {
            let mut req = request();
            req.kind = kind;
            let json = serde_json::to_value(&req).unwrap();
            assert_eq!(serde_json::from_value::<RuntimeRequest>(json).unwrap(), req);
        }
        let rule = ApprovalRule::new(RuleScope::Global, RuleEffect::Deny);
        let json = serde_json::to_value(&rule).unwrap();
        assert_eq!(json["scope"], "global");
        assert_eq!(
            serde_json::from_value::<ApprovalRule>(json.clone()).unwrap(),
            rule
        );

        let bot_rule = ApprovalRule::new(RuleScope::Bot(BotId::new()), RuleEffect::Ask);
        let json = serde_json::to_value(&bot_rule).unwrap();
        assert!(json["scope"]["bot"].is_string(), "{json}");
        assert_eq!(
            serde_json::from_value::<ApprovalRule>(json).unwrap(),
            bot_rule
        );
    }
}

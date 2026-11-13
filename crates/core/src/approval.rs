//! Approvals: rules first, a human second, and never a blocked tool call.
//!
//! Two shapes share one lifecycle:
//!
//! * **blocking** — a tool needs a decision to continue ([`ApprovalService::ask`],
//!   reached through [`ApprovalBridge`], which implements the tools crate's
//!   `ApprovalGate`);
//! * **async** — `ask_user` and `request_approval` return `{status: pending, id}`
//!   at once ([`ApprovalService::raise_async`]); the answer arrives later as a
//!   delivery that wakes the bot.
//!
//! The rule order is fixed and deliberately paranoid: a bot's *deny* beats its
//! *ask*, which beats its *allow*; the same order applies globally underneath;
//! anything unmatched is *ask*. "Allow always" writes a rule, so the same
//! question is never asked twice. Unattended requests expire — a card nobody
//! answered must not hold a turn open forever.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::oneshot;
use tracing::{debug, warn};

use swarmbot_contracts::config::Settings;
use swarmbot_contracts::delivery::Delivery;
use swarmbot_contracts::entry::NoticeLevel;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::{BotId, RequestId, RuleId, TurnId};
use swarmbot_contracts::repo::Persistence;
use swarmbot_contracts::request::{
    ApprovalRule, Decision, RequestStatus, Responder, RuleEffect, RuleScope, RuntimeRequest,
};
use swarmbot_contracts::tool::ToolResult;
use swarmbot_tools::{ApprovalAsk, ApprovalGate, ToolContext};

use crate::bus::{Change, LiveHub};
use crate::runners::RunnerMap;
use crate::settings::SettingsHandle;
use crate::transcript::Transcript;

/// How often due requests are expired when nobody is watching.
const EXPIRY_TICK: Duration = Duration::from_secs(15);

/// How a request was settled.
#[derive(Clone, Debug, PartialEq)]
pub struct Settled {
    pub status: RequestStatus,
    pub decision: Option<Decision>,
    pub answer: Option<Value>,
    pub by: Responder,
}

/// What the caller of a blocking ask may do next.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Allowed {
        rule_id: Option<RuleId>,
    },
    Denied {
        reason: String,
    },
    /// The turn was stopped while the card waited.
    Cancelled,
}

impl Outcome {
    pub fn allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }

    /// The tool result a denial becomes.
    pub fn to_result(&self) -> ToolResult {
        match self {
            Self::Allowed { .. } => ToolResult::text("allowed"),
            Self::Denied { reason } => ToolResult::error(format!("denied: {reason}")),
            Self::Cancelled => ToolResult::error("cancelled: the turn was stopped"),
        }
    }
}

/// Which rule spoke, and what it said.
#[derive(Clone, Debug, PartialEq)]
pub struct Evaluation {
    pub effect: RuleEffect,
    pub rule_id: Option<RuleId>,
}

pub struct ApprovalService {
    store: Arc<dyn Persistence>,
    hub: Arc<LiveHub>,
    transcript: Arc<Transcript>,
    settings: Arc<SettingsHandle>,
    runners: Arc<RunnerMap>,
    /// Blocking asks wait here. An ask with no waiter is an async one: its
    /// answer has to be delivered as a message instead.
    waiters: Mutex<HashMap<RequestId, oneshot::Sender<Settled>>>,
}

impl ApprovalService {
    pub fn new(
        store: Arc<dyn Persistence>,
        hub: Arc<LiveHub>,
        transcript: Arc<Transcript>,
        settings: Arc<SettingsHandle>,
        runners: Arc<RunnerMap>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            hub,
            transcript,
            settings,
            runners,
            waiters: Mutex::new(HashMap::new()),
        })
    }

    /// The rule that decides this request, in the documented order.
    ///
    /// One thing overrides every rule: a **destructive shell command always
    /// asks**. An `Allow` rule is a statement about ordinary work, and the
    /// only thing a saved prefix can honestly promise is "the commands I
    /// answered for" — it can never promise `rm -rf /`. Downgrading here (not
    /// only when the rule is written) also repairs rules written by an older
    /// build, which stored bare first-word prefixes.
    pub fn evaluate(&self, request: &RuntimeRequest) -> Evaluation {
        let rules = self.store.list_rules().unwrap_or_default();
        let destructive = is_destructive_request(request);
        for scope in [ScopeFilter::Bot(&request.bot_id), ScopeFilter::Global] {
            for effect in [RuleEffect::Deny, RuleEffect::Ask, RuleEffect::Allow] {
                if let Some(rule) = rules.iter().find(|rule| {
                    scope.matches(rule) && rule.effect == effect && rule.matches(request)
                }) {
                    let effect = if destructive && effect == RuleEffect::Allow {
                        RuleEffect::Ask
                    } else {
                        effect
                    };
                    return Evaluation {
                        effect,
                        rule_id: Some(rule.id.clone()),
                    };
                }
            }
        }
        // Nothing matched: ask. A default that runs is a default that surprises.
        Evaluation {
            effect: RuleEffect::Ask,
            rule_id: None,
        }
    }

    /// Ask, and wait for the answer or the expiry.
    pub async fn ask(&self, mut request: RuntimeRequest) -> Result<Outcome, DomainError> {
        let settings = self.settings.get();

        match self.evaluate(&request) {
            Evaluation {
                effect: RuleEffect::Allow,
                rule_id,
            } => {
                self.record_rule_decision(&request, "allowed by a rule");
                return Ok(Outcome::Allowed { rule_id });
            }
            Evaluation {
                effect: RuleEffect::Deny,
                rule_id,
            } => {
                self.record_rule_decision(&request, "blocked by your rules");
                let _ = rule_id;
                return Ok(Outcome::Denied {
                    reason: "blocked by your rules".into(),
                });
            }
            Evaluation {
                effect: RuleEffect::Ask,
                ..
            } => {
                // Only now may convenience speak: a read-only action skips the
                // card when nothing forbade it. This used to be checked before
                // `evaluate`, which silently turned every `deny` rule for
                // `read_file` into dead code.
                if request.kind == swarmbot_contracts::request::RequestKind::Approval
                    && settings.auto_approve_read_only
                    && self.is_read_only(&request)
                {
                    return Ok(Outcome::Allowed { rule_id: None });
                }
            }
        }

        request.expires_at = Some(
            request.created_at + chrono::Duration::seconds(settings.approval_expiry_secs as i64),
        );
        self.raise(&mut request)?;

        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(request.id.clone(), tx);

        let deadline = request.expires_at.unwrap_or_else(Utc::now);
        let wait = deadline
            .signed_duration_since(Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(settled)) => Ok(self.outcome_of(&settled)),
            // The expiry ticker beat us to it, or the waiter was dropped.
            Ok(Err(_)) | Err(_) => {
                let stored = self.store.get_request(&request.id)?;
                match stored {
                    Some(stored) if stored.status == RequestStatus::Answered => {
                        Ok(self.outcome_of(&settled_of(&stored)))
                    }
                    _ => {
                        self.expire_one(&request.id)?;
                        Ok(Outcome::Denied {
                            reason: "nobody answered in time".into(),
                        })
                    }
                }
            }
        }
    }

    /// Raise a card and return at once: `ask_user` and `request_approval` must
    /// never block a tool call.
    pub fn raise_async(&self, request: &mut RuntimeRequest) -> Result<RequestId, DomainError> {
        self.raise(request)?;
        Ok(request.id.clone())
    }

    /// Insert, card, publish. Shared by both shapes so a card always exists.
    fn raise(&self, request: &mut RuntimeRequest) -> Result<(), DomainError> {
        self.store.insert_request(request)?;
        let body = card_body(request);
        let card = self.transcript.card(request, request.title(), body)?;
        request.card_entry_id = Some(card.id.clone());
        self.store.update_request(request)?;
        self.hub.publish(Change::Request(request.clone()));
        debug!(request = %request.id, kind = ?request.kind, "a request is waiting");
        Ok(())
    }

    /// Answer a pending request. Called by the API (the owner) and by rules.
    pub fn respond(
        &self,
        id: &RequestId,
        decision: Option<Decision>,
        answer: Option<Value>,
        by: Responder,
    ) -> Result<RuntimeRequest, DomainError> {
        let mut request = self
            .store
            .get_request(id)?
            .ok_or_else(|| swarmbot_contracts::not_found::request(id))?;
        if !request.is_pending() {
            return Err(DomainError::RequestNotPending {
                request_id: id.clone(),
            });
        }
        let now = Utc::now();
        if !request.respond(now, decision, answer.clone(), by.clone()) {
            return Err(DomainError::RequestNotPending {
                request_id: id.clone(),
            });
        }
        self.store.update_request(&request)?;
        self.update_card(&request)?;
        self.hub.publish(Change::Request(request.clone()));

        // "Always allow" is the moment a rule is born — except for a
        // destructive command, where the answer covers this call and nothing
        // else (see [`rule_from`]).
        if decision == Some(Decision::AllowAlways) {
            match rule_from(&request) {
                Some(rule) => {
                    self.store.insert_rule(&rule)?;
                    let _ = self.transcript.event(
                        &request.conversation_id,
                        "rule-added",
                        format!("Always allowing {} for this bot", rule_description(&rule)),
                    );
                }
                None => {
                    let _ = self.transcript.event(
                        &request.conversation_id,
                        "rule-added",
                        "Allowed this once: a destructive command never becomes a standing rule"
                            .to_string(),
                    );
                }
            }
        }

        let settled = settled_of(&request);
        match self.take_waiter(id) {
            Some(waiter) => {
                let _ = waiter.send(settled);
            }
            // Nobody was blocking: the bot has to be told, or it waits forever.
            None => self.deliver_answer(&request, &settled),
        }
        Ok(request)
    }

    /// Expire every request whose time is up. Returns how many.
    pub fn expire_due(&self, now: DateTime<Utc>) -> Result<usize, DomainError> {
        let mut count = 0;
        for request in self.store.pending_requests()? {
            if request.is_expired(now) {
                self.expire_one(&request.id)?;
                count += 1;
            }
        }
        Ok(count)
    }

    fn expire_one(&self, id: &RequestId) -> Result<(), DomainError> {
        let Some(mut request) = self.store.get_request(id)? else {
            return Ok(());
        };
        if !request.is_pending() {
            return Ok(());
        }
        let now = Utc::now();
        request.expire(now);
        self.store.update_request(&request)?;
        self.update_card(&request)?;
        self.hub.publish(Change::Request(request.clone()));
        let settled = settled_of(&request);
        match self.take_waiter(id) {
            Some(waiter) => {
                let _ = waiter.send(settled);
            }
            None => self.deliver_answer(&request, &settled),
        }
        Ok(())
    }

    /// A stopped turn must not leave its cards waiting.
    pub fn cancel_turn(&self, turn: &TurnId) -> Result<usize, DomainError> {
        let mut count = 0;
        for mut request in self.store.pending_requests()? {
            if request.turn_id.as_ref() == Some(turn) {
                request.cancel(Utc::now(), "the turn was stopped");
                self.store.update_request(&request)?;
                self.update_card(&request)?;
                self.hub.publish(Change::Request(request.clone()));
                if let Some(waiter) = self.take_waiter(&request.id) {
                    let _ = waiter.send(Settled {
                        status: RequestStatus::Cancelled,
                        decision: None,
                        answer: None,
                        by: Responder::System {
                            reason: "the turn was stopped".into(),
                        },
                    });
                }
                count += 1;
            }
        }
        Ok(count)
    }

    fn take_waiter(&self, id: &RequestId) -> Option<oneshot::Sender<Settled>> {
        self.waiters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
    }

    fn outcome_of(&self, settled: &Settled) -> Outcome {
        match settled.status {
            RequestStatus::Answered => match settled.decision {
                Some(Decision::Deny) => Outcome::Denied {
                    reason: "the owner said no".into(),
                },
                Some(Decision::AllowOnce) | Some(Decision::AllowAlways) => {
                    Outcome::Allowed { rule_id: None }
                }
                // No decision means it was a question: the content is the answer.
                None => Outcome::Allowed { rule_id: None },
            },
            RequestStatus::Expired => Outcome::Denied {
                reason: "nobody answered in time".into(),
            },
            RequestStatus::Cancelled => Outcome::Cancelled,
            RequestStatus::Pending => Outcome::Cancelled,
        }
    }

    /// Tell the bot what happened, as a message it will be woken by.
    fn deliver_answer(&self, request: &RuntimeRequest, settled: &Settled) {
        let text = match settled.status {
            RequestStatus::Answered => {
                let answer = settled
                    .answer
                    .as_ref()
                    .map(|value| match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| {
                        match settled.decision {
                            Some(Decision::Deny) => "denied",
                            Some(_) => "allowed",
                            None => "answered",
                        }
                        .to_string()
                    });
                format!(
                    "[Answer to your request {}] {answer}",
                    short_id(&request.id)
                )
            }
            RequestStatus::Expired => format!(
                "[Your request {} expired] Nobody answered in time. Carry on without it, or ask \
                 again if it still matters.",
                short_id(&request.id)
            ),
            other => format!(
                "[Your request {} was closed: {:?}]",
                short_id(&request.id),
                other
            ),
        };
        let delivery = Delivery::async_answer(
            request.conversation_id.clone(),
            request.bot_id.clone(),
            request.id.clone(),
            text,
        );
        if let Err(err) = self.runners.deliver(delivery) {
            warn!(error = %err, "cannot deliver an answer to its bot");
        }
    }

    fn update_card(&self, request: &RuntimeRequest) -> Result<(), DomainError> {
        let Some(card_id) = request.card_entry_id.as_ref() else {
            return Ok(());
        };
        let Some(mut entry) = self.store.get_entry(card_id)? else {
            return Ok(());
        };
        if let Some(card) = entry.as_card_mut() {
            card.status = request.status;
        }
        entry.revision += 1;
        entry.updated_at = Utc::now();
        self.transcript.update(&entry)
    }

    fn record_rule_decision(&self, request: &RuntimeRequest, what: &str) {
        // Only worth a line when a rule, not a default, decided it.
        if let Err(err) = self.transcript.event(
            &request.conversation_id,
            "rule-decision",
            format!("{what}: {}", request.target.clone().unwrap_or_default()),
        ) {
            debug!(error = %err, "cannot record a rule decision");
        }
    }

    fn is_read_only(&self, request: &RuntimeRequest) -> bool {
        use swarmbot_contracts::node::Surface;
        matches!(request.surface, Some(Surface::ReadFile))
    }
    // --- rules --------------------------------------------------------------

    pub fn rules(&self) -> Result<Vec<ApprovalRule>, DomainError> {
        self.store.list_rules()
    }

    pub fn add_rule(&self, rule: ApprovalRule) -> Result<ApprovalRule, DomainError> {
        self.store.insert_rule(&rule)?;
        Ok(rule)
    }

    pub fn delete_rule(&self, id: &RuleId) -> Result<(), DomainError> {
        self.store.delete_rule(id)
    }

    /// "Always allow" for one bot and surface, the way the card offers it.
    pub fn allow_always(
        &self,
        bot: &BotId,
        request: &RuntimeRequest,
    ) -> Result<ApprovalRule, DomainError> {
        let mut rule = ApprovalRule::new(RuleScope::Bot(bot.clone()), RuleEffect::Allow);
        rule.surface = request.surface;
        rule.tool = request.tool.clone();
        rule.command_prefix = command_prefix_of(request);
        self.add_rule(rule)
    }

    pub fn pending(&self) -> Result<Vec<RuntimeRequest>, DomainError> {
        self.store.pending_requests()
    }

    pub fn settings(&self) -> Settings {
        self.settings.get()
    }

    /// A task that expires due requests. Started by the brain; ends when the
    /// service is dropped.
    pub fn spawn_expiry_ticker(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(EXPIRY_TICK);
            loop {
                tick.tick().await;
                match service.expire_due(Utc::now()) {
                    Ok(0) => {}
                    Ok(n) => debug!(expired = n, "expired unattended requests"),
                    Err(err) => warn!(error = %err, "cannot expire requests"),
                }
            }
        })
    }

    /// A notice in the conversation, for the cases where a card is not enough.
    pub fn notice(&self, request: &RuntimeRequest, text: impl Into<String>) {
        if let Err(err) =
            self.transcript
                .notice(&request.conversation_id, text, NoticeLevel::Warning)
        {
            debug!(error = %err, "cannot post a request notice");
        }
    }

    pub fn store(&self) -> Arc<dyn Persistence> {
        Arc::clone(&self.store)
    }
}

/// Which scope a rule has to be in to be considered at this step.
#[derive(Clone, Copy)]
enum ScopeFilter<'a> {
    Bot(&'a BotId),
    Global,
}

impl ScopeFilter<'_> {
    fn matches(self, rule: &ApprovalRule) -> bool {
        match (self, &rule.scope) {
            (ScopeFilter::Bot(wanted), RuleScope::Bot(bot_id)) => bot_id == wanted,
            (ScopeFilter::Global, RuleScope::Global) => true,
            _ => false,
        }
    }
}

/// The bridge the tools crate calls. It knows nothing about tool internals: it
/// turns an ask into a request and the answer into a boolean.
pub struct ApprovalBridge {
    service: Arc<ApprovalService>,
}

impl ApprovalBridge {
    pub fn new(service: Arc<ApprovalService>) -> Arc<Self> {
        Arc::new(Self { service })
    }

    pub fn service(&self) -> Arc<ApprovalService> {
        Arc::clone(&self.service)
    }
}

#[async_trait::async_trait]
impl ApprovalGate for ApprovalBridge {
    async fn request(&self, ctx: &ToolContext, ask: ApprovalAsk) -> Result<bool, DomainError> {
        let mut request = RuntimeRequest::approval(
            ctx.invocation.bot_id.clone(),
            ctx.invocation.conversation_id.clone(),
            ask.surface,
            ask.tool.clone(),
            ask.target.clone(),
            ask.reason.clone(),
        );
        request.turn_id = ctx.invocation.turn_id.clone();
        request.task_id = ctx.invocation.task_id.clone();
        let outcome = self.service.ask(request).await?;
        Ok(outcome.allowed())
    }

    fn cancel_turn(&self, turn: &TurnId) -> Result<usize, DomainError> {
        self.service.cancel_turn(turn)
    }
}

/// The text a card shows.
fn card_body(request: &RuntimeRequest) -> String {
    let mut parts = Vec::new();
    if let Some(target) = &request.target {
        parts.push(format!("`{target}`"));
    }
    if let Some(reason) = &request.reason {
        parts.push(reason.clone());
    }
    if let Some(prompt) = &request.prompt {
        parts.push(prompt.clone());
    }
    if parts.is_empty() {
        "A bot needs your answer.".to_string()
    } else {
        parts.join("\n")
    }
}

/// The rule an "always allow" writes: as narrow as the request allows, so one
/// yes does not become a blanket one.
///
/// Returns `None` for a destructive shell command on purpose: no saved answer
/// may authorise one. The owner can still allow that single call; the next
/// `rm -rf` asks again. That is the honest reading of "always allow" — it
/// cannot be a promise about commands nobody has seen.
fn rule_from(request: &RuntimeRequest) -> Option<ApprovalRule> {
    if is_destructive_request(request) {
        return None;
    }
    let mut rule = ApprovalRule::new(RuleScope::Bot(request.bot_id.clone()), RuleEffect::Allow);
    rule.surface = request.surface;
    rule.tool = request.tool.clone();
    rule.command_prefix = command_prefix_of(request);
    Some(rule)
}

/// True when this request is a shell command the destructive matcher flags.
///
/// The matcher is a heuristic, not a shell parser (`tools::is_destructive` says
/// so itself); the direction of its error is what makes it safe here: a false
/// positive only costs the owner one extra card, a false negative would be a
/// silent permission.
fn is_destructive_request(request: &RuntimeRequest) -> bool {
    use swarmbot_contracts::node::Surface;
    match request.surface {
        Some(Surface::Shell) => request
            .target
            .as_deref()
            .is_some_and(swarmbot_tools::is_destructive),
        _ => false,
    }
}

/// For a command, the first word: "always allow `cargo`" is useful, "always
/// allow `cargo test --workspace -- --nocapture`" is not.
fn command_prefix_of(request: &RuntimeRequest) -> Option<String> {
    use swarmbot_contracts::node::Surface;
    match request.surface {
        Some(Surface::Shell) => request
            .target
            .as_deref()
            .and_then(|target| target.split_whitespace().next())
            .map(str::to_string),
        _ => None,
    }
}

/// One line describing a rule, for the transcript and the settings list.
pub fn rule_description(rule: &ApprovalRule) -> String {
    let mut parts = Vec::new();
    if let Some(surface) = rule.surface {
        parts.push(surface.to_string());
    }
    if let Some(tool) = &rule.tool {
        parts.push(format!("`{tool}`"));
    }
    if let Some(prefix) = &rule.command_prefix {
        parts.push(format!("starting with `{prefix}`"));
    }
    if parts.is_empty() {
        "everything".to_string()
    } else {
        parts.join(" ")
    }
}

fn settled_of(request: &RuntimeRequest) -> Settled {
    Settled {
        status: request.status,
        decision: request.decision,
        answer: request.answer.clone(),
        by: request.responded_by.clone().unwrap_or(Responder::System {
            reason: "unsettled".into(),
        }),
    }
}

fn short_id(id: &RequestId) -> String {
    let raw = id.as_str();
    raw.rsplit('_').next().unwrap_or(raw)[..6.min(raw.len())].to_string()
}

impl ApprovalService {
    /// Test handles: the parts a test has to reach without a full brain.
    #[cfg(test)]
    pub fn runners_for_test(&self) -> &RunnerMap {
        &self.runners
    }

    #[cfg(test)]
    pub fn settings_handle_for_test(&self) -> &SettingsHandle {
        &self.settings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runners::NoHooks;
    use crate::testing;
    use swarmbot_contracts::node::Surface;
    use swarmbot_contracts::request::{FormField, RequestKind};

    struct Rig {
        service: Arc<ApprovalService>,
        store: Arc<dyn Persistence>,
        bot: swarmbot_contracts::bot::Bot,
        conversation: swarmbot_contracts::conversation::Conversation,
    }

    fn rig() -> Rig {
        let fixture = testing::Fixture::memory();
        let bot = fixture.bot("lead");
        let conversation = fixture
            .store
            .get_conversation(&bot.direct_conversation_id)
            .unwrap()
            .unwrap();
        let settings = SettingsHandle::new(fixture.store.clone());
        let runners = RunnerMap::new();
        let service = ApprovalService::new(
            fixture.store.clone(),
            fixture.hub.clone(),
            fixture.transcript.clone(),
            settings,
            runners,
        );
        let _ = NoHooks;
        Rig {
            service,
            store: fixture.store,
            bot,
            conversation,
        }
    }

    fn approval(rig: &Rig, target: &str) -> RuntimeRequest {
        RuntimeRequest::approval(
            rig.bot.id.clone(),
            rig.conversation.id.clone(),
            Surface::Shell,
            "shell",
            target,
            Some("run the tests".into()),
        )
    }

    fn question(rig: &Rig) -> RuntimeRequest {
        RuntimeRequest::question(
            rig.bot.id.clone(),
            rig.conversation.id.clone(),
            "which environment?",
            vec![FormField::text("env", "Environment").required()],
        )
    }

    #[test]
    fn the_default_is_to_ask() {
        let rig = rig();
        let request = approval(&rig, "cargo test");
        assert_eq!(rig.service.evaluate(&request).effect, RuleEffect::Ask);
        assert_eq!(rig.service.evaluate(&request).rule_id, None);
    }

    #[test]
    fn a_bots_deny_beats_its_allow_and_a_global_allow() {
        let rig = rig();
        // An ordinary command on purpose: this test is about precedence, and a
        // destructive one would (correctly) be downgraded to Ask regardless.
        let request = approval(&rig, "cargo build");

        let mut allow = ApprovalRule::new(RuleScope::Bot(rig.bot.id.clone()), RuleEffect::Allow);
        allow.surface = Some(Surface::Shell);
        rig.store.insert_rule(&allow).unwrap();
        assert_eq!(rig.service.evaluate(&request).effect, RuleEffect::Allow);

        let mut global_allow = ApprovalRule::new(RuleScope::Global, RuleEffect::Allow);
        global_allow.surface = Some(Surface::Shell);
        rig.store.insert_rule(&global_allow).unwrap();
        assert_eq!(rig.service.evaluate(&request).effect, RuleEffect::Allow);

        // A bot-scoped deny outranks both.
        let mut deny = ApprovalRule::new(RuleScope::Bot(rig.bot.id.clone()), RuleEffect::Deny);
        deny.surface = Some(Surface::Shell);
        deny.command_prefix = Some("cargo".into());
        rig.store.insert_rule(&deny).unwrap();
        let evaluation = rig.service.evaluate(&request);
        assert_eq!(evaluation.effect, RuleEffect::Deny);
        assert_eq!(evaluation.rule_id, Some(deny.id.clone()));
    }

    #[test]
    fn another_bots_rule_never_applies() {
        let rig = rig();
        let other = testing::seed_bot(&rig.store, "scout");
        let mut rule = ApprovalRule::new(RuleScope::Bot(other.id.clone()), RuleEffect::Allow);
        rule.surface = Some(Surface::Shell);
        rig.store.insert_rule(&rule).unwrap();
        assert_eq!(
            rig.service.evaluate(&approval(&rig, "cargo test")).effect,
            RuleEffect::Ask
        );
    }

    #[test]
    fn a_command_prefix_narrows_a_rule() {
        let rig = rig();
        let mut rule = ApprovalRule::new(RuleScope::Global, RuleEffect::Allow);
        rule.surface = Some(Surface::Shell);
        rule.command_prefix = Some("cargo".into());
        rig.store.insert_rule(&rule).unwrap();
        assert_eq!(
            rig.service.evaluate(&approval(&rig, "cargo test")).effect,
            RuleEffect::Allow
        );
        assert_eq!(
            rig.service.evaluate(&approval(&rig, "rm -rf /")).effect,
            RuleEffect::Ask
        );
    }

    #[tokio::test]
    async fn a_read_only_ask_is_allowed_without_a_card() {
        let rig = rig();
        let mut request = approval(&rig, "/tmp/x");
        request.surface = Some(Surface::ReadFile);
        let outcome = rig.service.ask(request).await.unwrap();
        assert!(outcome.allowed());
        assert!(
            rig.service.pending().unwrap().is_empty(),
            "no card was raised"
        );
    }

    #[tokio::test]
    async fn an_allowed_rule_never_raises_a_card() {
        let rig = rig();
        let mut rule = ApprovalRule::new(RuleScope::Global, RuleEffect::Allow);
        rule.surface = Some(Surface::Shell);
        rig.store.insert_rule(&rule).unwrap();
        let outcome = rig.service.ask(approval(&rig, "cargo test")).await.unwrap();
        assert_eq!(
            outcome,
            Outcome::Allowed {
                rule_id: Some(rule.id)
            }
        );
        assert!(rig.service.pending().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_denied_rule_says_why() {
        let rig = rig();
        let mut rule = ApprovalRule::new(RuleScope::Global, RuleEffect::Deny);
        rule.surface = Some(Surface::Shell);
        rig.store.insert_rule(&rule).unwrap();
        let outcome = rig.service.ask(approval(&rig, "sudo rm")).await.unwrap();
        assert_eq!(
            outcome,
            Outcome::Denied {
                reason: "blocked by your rules".into()
            }
        );
        assert!(outcome.to_result().is_error);
        assert!(
            outcome
                .to_result()
                .text_only()
                .starts_with("ERROR: denied: blocked by your rules")
        );
    }

    #[tokio::test]
    async fn an_ask_raises_a_card_and_resolves_when_answered() {
        let rig = rig();
        let request = approval(&rig, "cargo test");
        let id = request.id.clone();
        let service = rig.service.clone();
        let asked = tokio::spawn(async move { service.ask(request).await });

        // The card exists and is pending.
        let mut pending = Vec::new();
        for _ in 0..100 {
            pending = rig.service.pending().unwrap();
            if !pending.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(pending.len(), 1, "one card is waiting");
        let card = pending[0].card_entry_id.clone().expect("a card entry");
        let entry = rig.store.get_entry(&card).unwrap().unwrap();
        assert_eq!(entry.kind.kind_str(), "card");

        rig.service
            .respond(&id, Some(Decision::AllowOnce), None, Responder::Owner)
            .unwrap();
        let outcome = asked.await.unwrap().unwrap();
        assert!(outcome.allowed());

        let settled = rig.store.get_request(&id).unwrap().unwrap();
        assert_eq!(settled.status, RequestStatus::Answered);
        let card = rig.store.get_entry(&card).unwrap().unwrap();
        assert_eq!(card.as_card().unwrap().status, RequestStatus::Answered);
    }

    #[tokio::test]
    async fn allow_always_writes_a_rule_narrow_enough_to_be_useful() {
        let rig = rig();
        let mut request = approval(&rig, "cargo test --workspace");
        rig.service.raise_async(&mut request).unwrap();
        rig.service
            .respond(
                &request.id,
                Some(Decision::AllowAlways),
                None,
                Responder::Owner,
            )
            .unwrap();
        let rules = rig.service.rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].effect, RuleEffect::Allow);
        assert_eq!(rules[0].surface, Some(Surface::Shell));
        assert_eq!(
            rules[0].command_prefix.as_deref(),
            Some("cargo"),
            "the first word, not the whole command"
        );
        // The same command is now allowed without a card.
        let outcome = rig
            .service
            .ask(approval(&rig, "cargo build"))
            .await
            .unwrap();
        assert!(outcome.allowed());
    }

    #[tokio::test]
    async fn an_answer_to_an_async_ask_wakes_the_bot_with_a_message() {
        let rig = rig();
        let mut request = question(&rig);
        let id = rig.service.raise_async(&mut request).unwrap();
        // No runner is registered, so the answer is parked for it.
        rig.service
            .respond(
                &id,
                None,
                Some(Value::String("staging".into())),
                Responder::Owner,
            )
            .unwrap();
        assert_eq!(rig.service.runners_for_test().parked_count(), 1);
    }

    #[tokio::test]
    async fn an_expired_request_is_denied_and_recorded() {
        let rig = rig();
        let mut request = approval(&rig, "cargo test");
        request.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        rig.service.raise_async(&mut request).unwrap();
        assert_eq!(rig.service.expire_due(Utc::now()).unwrap(), 1);
        let stored = rig.store.get_request(&request.id).unwrap().unwrap();
        assert_eq!(stored.status, RequestStatus::Expired);
        assert_eq!(stored.responded_by, Some(Responder::Expiry));
        // Expiring twice changes nothing.
        assert_eq!(rig.service.expire_due(Utc::now()).unwrap(), 0);
    }

    #[tokio::test]
    async fn a_blocking_ask_times_out_into_a_deny() {
        let rig = rig();
        rig.service
            .settings_handle_for_test()
            .update(|settings| settings.approval_expiry_secs = 0)
            .unwrap();
        let outcome = rig.service.ask(approval(&rig, "cargo test")).await.unwrap();
        assert_eq!(
            outcome,
            Outcome::Denied {
                reason: "nobody answered in time".into()
            }
        );
    }

    #[tokio::test]
    async fn stopping_a_turn_closes_its_cards() {
        let rig = rig();
        let mut request = approval(&rig, "cargo test");
        request.turn_id = Some(TurnId::new());
        let turn = request.turn_id.clone().unwrap();
        rig.service.raise_async(&mut request).unwrap();
        assert_eq!(rig.service.cancel_turn(&turn).unwrap(), 1);
        let stored = rig.store.get_request(&request.id).unwrap().unwrap();
        assert_eq!(stored.status, RequestStatus::Cancelled);
        assert_eq!(rig.service.cancel_turn(&turn).unwrap(), 0);
    }

    #[test]
    fn answering_a_settled_request_is_refused() {
        let rig = rig();
        let mut request = question(&rig);
        rig.service.raise_async(&mut request).unwrap();
        rig.service
            .respond(
                &request.id,
                None,
                Some(Value::String("a".into())),
                Responder::Owner,
            )
            .unwrap();
        let err = rig
            .service
            .respond(
                &request.id,
                None,
                Some(Value::String("b".into())),
                Responder::Owner,
            )
            .unwrap_err();
        assert_eq!(err.code(), "request_not_pending");
    }

    #[test]
    fn a_missing_request_is_not_found() {
        let rig = rig();
        let err = rig
            .service
            .respond(
                &RequestId::new(),
                Some(Decision::Deny),
                None,
                Responder::Owner,
            )
            .unwrap_err();
        assert_eq!(err.code(), "not_found");
    }

    #[test]
    fn card_bodies_read_on_their_own() {
        let rig = rig();
        let request = approval(&rig, "cargo test");
        assert_eq!(card_body(&request), "`cargo test`\nrun the tests");
        let bare = question(&rig);
        assert_eq!(card_body(&bare), "which environment?");
        assert_eq!(bare.kind, RequestKind::Question);
    }

    // -- the destructive floor -------------------------------------------------

    #[test]
    fn a_saved_allow_rule_cannot_authorise_a_destructive_command() {
        // One "always allow `rm build/x`" stores the prefix `rm`. Before the
        // boundary fix that rule matched `rm -rf /` and the destructive floor
        // in the policy engine was undone by the gate.
        let rig = rig();
        let mut rule = ApprovalRule::new(RuleScope::Bot(rig.bot.id.clone()), RuleEffect::Allow);
        rule.surface = Some(Surface::Shell);
        rule.tool = Some("shell".into());
        rule.command_prefix = Some("rm".into());
        rig.store.insert_rule(&rule).unwrap();

        let destructive = approval(&rig, "rm -rf /");
        let evaluation = rig.service.evaluate(&destructive);
        assert_eq!(
            evaluation.effect,
            RuleEffect::Ask,
            "a destructive command always asks, whatever a saved rule says"
        );

        // The same rule still covers ordinary work under that prefix.
        let ordinary = approval(&rig, "rm build/artifact.o");
        assert_eq!(rig.service.evaluate(&ordinary).effect, RuleEffect::Allow);
    }

    #[test]
    fn allow_always_on_a_destructive_command_writes_no_rule() {
        let rig = rig();
        let mut request = approval(&rig, "rm -rf /tmp/scratch");
        rig.service.raise_async(&mut request).unwrap();
        rig.service
            .respond(
                &request.id,
                Some(Decision::AllowAlways),
                None,
                Responder::Owner,
            )
            .unwrap();

        assert!(
            rig.service.rules().unwrap().is_empty(),
            "a destructive command is allowed once, never standing"
        );
    }

    #[test]
    fn a_deny_rule_beats_auto_approve_read_only() {
        // `auto_approve_read_only` defaults to true. It used to be checked
        // before the rules, which made every `deny` rule for `read_file` dead
        // code — the one lever an owner has to forbid reading something.
        let rig = rig();
        let mut rule = ApprovalRule::new(RuleScope::Bot(rig.bot.id.clone()), RuleEffect::Deny);
        rule.surface = Some(Surface::ReadFile);
        rig.store.insert_rule(&rule).unwrap();

        let request = RuntimeRequest::approval(
            rig.bot.id.clone(),
            rig.conversation.id.clone(),
            Surface::ReadFile,
            "read_file",
            "/etc/shadow",
            Some("read a file".into()),
        );
        let evaluation = rig.service.evaluate(&request);
        assert_eq!(evaluation.effect, RuleEffect::Deny);
        assert!(evaluation.rule_id.is_some());
    }
}

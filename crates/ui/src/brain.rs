//! The brain seam: a [`Brain`] trait the worker talks to, one real
//! implementation over [`swarmbot_client::BrainClient`], and (test-only) a
//! scripted in-memory double.
//!
//! [`../../../docs/ui.md`] §6 asks for `swarmbot-client` to sit behind a trait so
//! a scripted double can stand in for it. That is exactly this module: the
//! worker's RPC thread holds an `Arc<dyn Brain>` and never names
//! [`swarmbot_client::BrainClient`] directly, so a test can drive command
//! dispatch with no socket. The live `/ws/live` socket is *not* behind this
//! trait — [`swarmbot_client::LiveSubscription`] is a concrete blocking socket
//! with no in-memory shape, and the live thread is only ever used against a real
//! (or spawned) brain; the reducer that consumes its events is tested directly
//! with fabricated [`swarmbot_contracts::LiveEvent`]s instead.

use serde_json::Value;
use swarmbot_client::{
    BackendModels, BackendProbe, BackendView, BrainClient, ClientError, Health, NewRoutineRequest,
    PairingCode, SentMessage, SessionAction, TaskDetail,
};
use swarmbot_contracts::{
    Bot, BotId, Conversation, ConversationId, Decision, EntryId, NodeId, NodeInfo, RequestId,
    Routine, RoutineId, RoutineRun, RuntimeRequest, Settings, Task, TaskId,
};

/// The REST surface the UI uses. Object-safe and `Send + Sync` so the worker
/// threads can share one `Arc<dyn Brain>`.
pub trait Brain: Send + Sync {
    fn health(&self) -> Result<Health, ClientError>;
    fn login(&self, one_time_token: &str) -> Result<(), ClientError>;
    fn send_message(
        &self,
        id: &ConversationId,
        text: &str,
        addressed: &[BotId],
        reply_to: Option<&EntryId>,
    ) -> Result<SentMessage, ClientError>;
    fn interrupt(&self, id: &ConversationId) -> Result<bool, ClientError>;
    fn respond_request(
        &self,
        id: &RequestId,
        decision: Option<Decision>,
        answer: Option<Value>,
    ) -> Result<RuntimeRequest, ClientError>;
    fn cancel_task(&self, id: &TaskId) -> Result<Task, ClientError>;
    fn task(&self, id: &TaskId) -> Result<TaskDetail, ClientError>;
    fn create_group(
        &self,
        title: &str,
        members: &[BotId],
        lead: Option<&BotId>,
    ) -> Result<Conversation, ClientError>;
    fn bot_session(&self, id_or_name: &str, action: SessionAction) -> Result<usize, ClientError>;
    fn create_pairing_code(&self) -> Result<PairingCode, ClientError>;
    fn revoke_node(&self, id: &NodeId) -> Result<(), ClientError>;
    fn settings(&self) -> Result<Settings, ClientError>;
    fn update_settings(&self, patch: &Value) -> Result<Settings, ClientError>;
    fn backends(&self) -> Result<Vec<BackendView>, ClientError>;
    fn probe_backends(&self) -> Result<Vec<BackendProbe>, ClientError>;
    /// PATCH one bot (the provider menu switches `backend` / `model`).
    fn update_bot(&self, id_or_name: &str, patch: &Value) -> Result<Bot, ClientError>;
    /// The models one backend advertises (empty = it offers no menu).
    fn models(&self, backend: &str) -> Result<BackendModels, ClientError>;
    fn nodes(&self) -> Result<Vec<NodeInfo>, ClientError>;
    fn routines(&self, bot: Option<&BotId>) -> Result<Vec<Routine>, ClientError>;
    fn create_routine(&self, request: &NewRoutineRequest) -> Result<Routine, ClientError>;
    fn pause_routine(&self, id: &RoutineId) -> Result<Routine, ClientError>;
    fn resume_routine(&self, id: &RoutineId) -> Result<Routine, ClientError>;
    fn delete_routine(&self, id: &RoutineId) -> Result<(), ClientError>;
    fn routine_runs(
        &self,
        id: &RoutineId,
        limit: Option<u32>,
    ) -> Result<Vec<RoutineRun>, ClientError>;
    fn shutdown(&self) -> Result<(), ClientError>;
}

/// The real brain, over the blocking client. Cheap to clone (the client shares
/// its connection pool and token), and `Send + Sync` because [`BrainClient`] is.
pub struct ClientBrain {
    client: BrainClient,
}

impl ClientBrain {
    /// Wrap an already-built client (token applied by the caller).
    pub fn new(client: BrainClient) -> Self {
        Self { client }
    }

    /// The underlying client, for the live thread (which needs `live()`, a
    /// method that returns a concrete socket and so is not on the trait).
    pub fn client(&self) -> &BrainClient {
        &self.client
    }
}

impl Brain for ClientBrain {
    fn health(&self) -> Result<Health, ClientError> {
        self.client.health()
    }
    fn login(&self, one_time_token: &str) -> Result<(), ClientError> {
        self.client.login(one_time_token)
    }
    fn send_message(
        &self,
        id: &ConversationId,
        text: &str,
        addressed: &[BotId],
        reply_to: Option<&EntryId>,
    ) -> Result<SentMessage, ClientError> {
        self.client.send_message(id, text, addressed, reply_to)
    }
    fn interrupt(&self, id: &ConversationId) -> Result<bool, ClientError> {
        self.client.interrupt(id)
    }
    fn respond_request(
        &self,
        id: &RequestId,
        decision: Option<Decision>,
        answer: Option<Value>,
    ) -> Result<RuntimeRequest, ClientError> {
        self.client.respond_request(id, decision, answer)
    }
    fn cancel_task(&self, id: &TaskId) -> Result<Task, ClientError> {
        self.client.cancel_task(id)
    }
    fn task(&self, id: &TaskId) -> Result<TaskDetail, ClientError> {
        self.client.task(id)
    }
    fn create_group(
        &self,
        title: &str,
        members: &[BotId],
        lead: Option<&BotId>,
    ) -> Result<Conversation, ClientError> {
        self.client.create_group(title, members, lead)
    }
    fn bot_session(&self, id_or_name: &str, action: SessionAction) -> Result<usize, ClientError> {
        self.client.bot_session(id_or_name, action)
    }
    fn create_pairing_code(&self) -> Result<PairingCode, ClientError> {
        self.client.create_pairing_code()
    }
    fn revoke_node(&self, id: &NodeId) -> Result<(), ClientError> {
        self.client.revoke_node(id)
    }
    fn settings(&self) -> Result<Settings, ClientError> {
        self.client.settings()
    }
    fn update_settings(&self, patch: &Value) -> Result<Settings, ClientError> {
        self.client.update_settings(patch)
    }
    fn backends(&self) -> Result<Vec<BackendView>, ClientError> {
        self.client.backends()
    }
    fn update_bot(&self, id_or_name: &str, patch: &Value) -> Result<Bot, ClientError> {
        self.client.update_bot(id_or_name, patch)
    }
    fn models(&self, backend: &str) -> Result<BackendModels, ClientError> {
        self.client.backend_models(backend)
    }
    fn probe_backends(&self) -> Result<Vec<BackendProbe>, ClientError> {
        self.client.probe_backends()
    }
    fn nodes(&self) -> Result<Vec<NodeInfo>, ClientError> {
        self.client.nodes()
    }
    fn routines(&self, bot: Option<&BotId>) -> Result<Vec<Routine>, ClientError> {
        self.client.routines(bot)
    }
    fn create_routine(&self, request: &NewRoutineRequest) -> Result<Routine, ClientError> {
        self.client.create_routine(request)
    }
    fn pause_routine(&self, id: &RoutineId) -> Result<Routine, ClientError> {
        self.client.pause_routine(id)
    }
    fn resume_routine(&self, id: &RoutineId) -> Result<Routine, ClientError> {
        self.client.resume_routine(id)
    }
    fn delete_routine(&self, id: &RoutineId) -> Result<(), ClientError> {
        self.client.delete_routine(id)
    }
    fn routine_runs(
        &self,
        id: &RoutineId,
        limit: Option<u32>,
    ) -> Result<Vec<RoutineRun>, ClientError> {
        self.client.routine_runs(id, limit)
    }
    fn shutdown(&self) -> Result<(), ClientError> {
        self.client.shutdown()
    }
}

/// A scripted, in-memory [`Brain`] for tests: every call succeeds with a
/// plausible value and is recorded, so a test can assert the exact wire shape a
/// button produced without opening a socket.
#[cfg(test)]
pub mod scripted {
    use super::*;
    use std::sync::Mutex;
    use swarmbot_contracts::{RequestStatus, Responder, Surface, TaskStatus};

    /// One recorded call, as a stable string a test can assert on.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Call {
        pub name: &'static str,
        pub detail: String,
    }

    #[derive(Default)]
    pub struct ScriptedBrain {
        calls: Mutex<Vec<Call>>,
        /// When set, every call fails with this error instead of succeeding.
        pub fail_with: Mutex<Option<ClientError>>,
    }

    impl ScriptedBrain {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        fn record(&self, name: &'static str, detail: impl Into<String>) {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Call {
                    name,
                    detail: detail.into(),
                });
        }

        fn maybe_fail<T>(&self, ok: T) -> Result<T, ClientError> {
            let mut guard = self.fail_with.lock().unwrap_or_else(|e| e.into_inner());
            match guard.take() {
                Some(err) => Err(err),
                None => Ok(ok),
            }
        }
    }

    impl Brain for ScriptedBrain {
        fn health(&self) -> Result<Health, ClientError> {
            self.record("health", "");
            self.maybe_fail(Health {
                ok: true,
                version: "test".into(),
                epoch: 1,
                uptime_secs: 1,
            })
        }
        fn login(&self, token: &str) -> Result<(), ClientError> {
            self.record("login", format!("{} chars", token.chars().count()));
            self.maybe_fail(())
        }
        fn send_message(
            &self,
            id: &ConversationId,
            text: &str,
            addressed: &[BotId],
            _reply_to: Option<&EntryId>,
        ) -> Result<SentMessage, ClientError> {
            self.record(
                "send_message",
                format!("{id} {:?} to {}", text, addressed.len()),
            );
            let entry = swarmbot_contracts::Entry::owner_message(id.clone(), 1, text.to_string());
            self.maybe_fail(SentMessage {
                entry,
                targets: addressed.to_vec(),
                queued: Vec::new(),
                reason: "lead".into(),
            })
        }
        fn interrupt(&self, id: &ConversationId) -> Result<bool, ClientError> {
            self.record("interrupt", id.to_string());
            self.maybe_fail(true)
        }
        fn respond_request(
            &self,
            id: &RequestId,
            decision: Option<Decision>,
            answer: Option<Value>,
        ) -> Result<RuntimeRequest, ClientError> {
            self.record(
                "respond_request",
                format!("{id} decision={decision:?} answer={answer:?}"),
            );
            let mut request = RuntimeRequest::approval(
                BotId::new(),
                ConversationId::new(),
                Surface::Shell,
                "shell",
                "true",
                None,
            );
            request.id = id.clone();
            request.status = RequestStatus::Answered;
            request.decision = decision;
            request.answer = answer;
            request.responded_by = Some(Responder::Owner);
            self.maybe_fail(request)
        }
        fn cancel_task(&self, id: &TaskId) -> Result<Task, ClientError> {
            self.record("cancel_task", id.to_string());
            let mut task = Task::own(BotId::new(), ConversationId::new(), "t", "b");
            task.id = id.clone();
            task.status = TaskStatus::Cancelled;
            self.maybe_fail(task)
        }
        fn task(&self, id: &TaskId) -> Result<TaskDetail, ClientError> {
            self.record("task", id.to_string());
            let mut task = Task::own(BotId::new(), ConversationId::new(), "t", "b");
            task.id = id.clone();
            self.maybe_fail(TaskDetail {
                task,
                steps: Vec::new(),
            })
        }
        fn create_group(
            &self,
            title: &str,
            members: &[BotId],
            _lead: Option<&BotId>,
        ) -> Result<Conversation, ClientError> {
            self.record("create_group", format!("{title} ({})", members.len()));
            let conversation = if members.len() >= 2 {
                Conversation::group(title, members.to_vec(), members[0].clone())
                    .unwrap_or_else(|_| Conversation::direct(members[0].clone()))
            } else {
                Conversation::direct(BotId::new())
            };
            self.maybe_fail(conversation)
        }
        fn bot_session(&self, id: &str, action: SessionAction) -> Result<usize, ClientError> {
            self.record("bot_session", format!("{id} {action:?}"));
            self.maybe_fail(0)
        }
        fn create_pairing_code(&self) -> Result<PairingCode, ClientError> {
            self.record("create_pairing_code", "");
            self.maybe_fail(PairingCode {
                code: "ABCD-EFGH".into(),
                expires_at: "2030-01-01T00:00:00Z".into(),
                node_id: None,
            })
        }
        fn revoke_node(&self, id: &NodeId) -> Result<(), ClientError> {
            self.record("revoke_node", id.to_string());
            self.maybe_fail(())
        }
        fn settings(&self) -> Result<Settings, ClientError> {
            self.record("settings", "");
            self.maybe_fail(Settings::default())
        }
        fn update_settings(&self, patch: &Value) -> Result<Settings, ClientError> {
            self.record("update_settings", patch.to_string());
            self.maybe_fail(Settings::default())
        }
        fn backends(&self) -> Result<Vec<BackendView>, ClientError> {
            self.record("backends", "");
            self.maybe_fail(vec![BackendView {
                id: "fake".into(),
                kind: swarmbot_contracts::BackendKind::Fake,
                base_url: None,
                model: Some("fake".into()),
                api_key_env: None,
                options: Default::default(),
            }])
        }
        fn models(&self, backend: &str) -> Result<BackendModels, ClientError> {
            self.record("models", backend.to_string());
            self.maybe_fail(BackendModels {
                models: vec![swarmbot_client::ModelOption {
                    id: "fake".into(),
                    label: None,
                }],
                authoritative: true,
            })
        }
        fn update_bot(&self, id_or_name: &str, patch: &Value) -> Result<Bot, ClientError> {
            self.record("update_bot", format!("{id_or_name} {patch}"));
            self.maybe_fail(swarmbot_contracts::Bot::new(
                "stub",
                "Stub",
                "",
                "fake",
                swarmbot_contracts::ConversationId::new(),
            ))
        }
        fn probe_backends(&self) -> Result<Vec<BackendProbe>, ClientError> {
            self.record("probe_backends", "");
            self.maybe_fail(vec![BackendProbe {
                backend: "fake".into(),
                ok: true,
                detail: Some("scripted".into()),
                model: Some("fake".into()),
                version: None,
            }])
        }
        fn nodes(&self) -> Result<Vec<NodeInfo>, ClientError> {
            self.record("nodes", "");
            self.maybe_fail(vec![NodeInfo::local(
                "here",
                vec![],
                swarmbot_contracts::PolicyMap::default(),
            )])
        }
        fn routines(&self, _bot: Option<&BotId>) -> Result<Vec<Routine>, ClientError> {
            self.record("routines", "");
            self.maybe_fail(Vec::new())
        }
        fn create_routine(&self, request: &NewRoutineRequest) -> Result<Routine, ClientError> {
            self.record(
                "create_routine",
                format!("{} for {}", request.name, request.bot),
            );
            let r = Routine::new(
                BotId::new(),
                &request.name,
                &request.prompt,
                request.schedule.clone(),
                chrono::Utc::now(),
            )
            .map_err(|e| ClientError::Http {
                status: 400,
                code: "invalid".into(),
                message: e.to_string(),
            })?;
            self.maybe_fail(r)
        }
        fn pause_routine(&self, id: &RoutineId) -> Result<Routine, ClientError> {
            self.record("pause_routine", id.to_string());
            let mut r = Routine::new(
                BotId::new(),
                "test",
                "test",
                swarmbot_contracts::RoutineSchedule::Interval { every_secs: 60 },
                chrono::Utc::now(),
            )
            .map_err(|e| ClientError::Http {
                status: 400,
                code: "invalid".into(),
                message: e.to_string(),
            })?;
            r.id = id.clone();
            r.pause();
            self.maybe_fail(r)
        }
        fn resume_routine(&self, id: &RoutineId) -> Result<Routine, ClientError> {
            self.record("resume_routine", id.to_string());
            let mut r = Routine::new(
                BotId::new(),
                "test",
                "test",
                swarmbot_contracts::RoutineSchedule::Interval { every_secs: 60 },
                chrono::Utc::now(),
            )
            .map_err(|e| ClientError::Http {
                status: 400,
                code: "invalid".into(),
                message: e.to_string(),
            })?;
            r.id = id.clone();
            let _ = r.resume(chrono::Utc::now());
            self.maybe_fail(r)
        }
        fn delete_routine(&self, id: &RoutineId) -> Result<(), ClientError> {
            self.record("delete_routine", id.to_string());
            self.maybe_fail(())
        }
        fn routine_runs(
            &self,
            id: &RoutineId,
            _limit: Option<u32>,
        ) -> Result<Vec<RoutineRun>, ClientError> {
            self.record("routine_runs", id.to_string());
            self.maybe_fail(Vec::new())
        }
        fn shutdown(&self) -> Result<(), ClientError> {
            self.record("shutdown", "");
            self.maybe_fail(())
        }
    }
}

# SwarmBot architecture

What the code on disk does, and where the seams are for what it does not do yet.
Binding decisions (and the reasons) live in [`decisions.md`](decisions.md); this
document describes the shape those decisions produced. Tool-level detail is in
[`tools.md`](tools.md), the node wire in [`node-protocol.md`](node-protocol.md),
the desktop app in [`ui.md`](ui.md).

State of the tree when this was written: **all twelve crates are built and
tested** — `contracts`, `store`, `llm`, `tools`, `core` (including the fourteen
bot-facing tools), `client`, `node-protocol`, `brain` (REST + `/ws/live` + owner
auth), `cli` (embedded *and* remote), `node` (the daemon), `ui` (the GPUI app) and
`mcp` (stdio + streamable HTTP, mounted at `/mcp` and `/mcp/t/{token}`). The
brain's **node gateway is real**
(`brain/src/gateway/`): pairing, the signed upgrade, the roster and a dispatcher
that routes a bot's tool call to the right machine — and
`crates/node/tests/real_brain.rs` pins an in-process brain against the **real
`swarmbot-node` binary**, including a remote tool call proven by a marker file on
the node and a decoy on the brain ([`node-protocol.md`](node-protocol.md) §11.1).
The last full `scripts/check.sh` was **1.283 passed, 0 failed, 4 ignored** (tests
289 s, 336 s total); the ignored ones are the live-gateway LLM test behind
`PROTO_LIVE_LLM`, the live ACP-agent test behind `PROTO_LIVE_ACP`, and two
doctests in `store/src/threaded.rs` that need a runtime a doctest does not have.

---

## 1. Crate dependency graph

```
  layer 4 (composition)   swarmbot-brain     swarmbot-cli      swarmbot-node    swarmbot-ui
                              │  │  │  │      │      │              │  │            │
  layer 3 (orchestration)     └──┼──┼──┼─ swarmbot-core ◄───────────┘  │            │
                                 │  │  │      │  │  │                   │            │
  layer 2 (adapters)   store ────┘  │  └─ llm │  └─ tools ◄─────────────┘            │
                           node-protocol ─────┘                                      │
  layer 1 (contracts)  swarmbot-contracts ◄──────────────────────────────── swarmbot-client
                                                                            (REST + /ws/live)
```

Read as: `brain` composes everything; `cli` runs `core` in-process for `--embedded`
and `client` for `--brain`; `node` composes `node-protocol` + `tools`; `ui` composes
`client` only. The one deliberate exception to the layering is a **dev**-dependency:
`crates/node`'s tests depend on `swarmbot-brain` — one to build a refusal envelope
with the brain's own `error_body` helper, one to compose a real brain in process —
so neither half can drift from what the other actually sends.

One direction, and the reason for each rule:

| Rule | Why |
| --- | --- |
| `contracts` depends on nothing but serde/uuid/chrono/thiserror | It is the spine. No I/O, no runtime, no database — so a wire type can never drag a database driver into a client. |
| `llm`, `tools`, `store`, `node-protocol` depend on `contracts` only | Each is an adapter behind a port. They cannot see each other, so a backend cannot reach into storage and a tool cannot call a model. |
| `core` depends on `llm`, `tools`, `store` | It is the only crate that knows a turn exists. It still never opens a socket, reads a config file or knows HTTP. |
| `brain`, `node`, `cli`, `ui` compose | Composition roots. `brain` mounts `core` on axum; `cli` runs the same `Swarm` in-process for `--embedded`. |
| `ui` depends on `client` + `contracts` only | GPUI owns the main thread and its own executor; the orchestration owns tokio. They meet over loopback HTTP, never in one process (D7.2). |
| `tools` does not know `core` | The way back to a human is the `ApprovalGate` port, and the way out to a machine is `NodeExecutor`. A node daemon embeds the same handlers without dragging the orchestration along. |

The two ports that make the last two rules work are declared in
`crates/tools/src/handler.rs`:

```rust
#[async_trait] pub trait ToolHandler   { fn spec(&self) -> ToolSpec; async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError>; }
#[async_trait] pub trait ApprovalGate  { async fn request(&self, ctx: &ToolContext, ask: ApprovalAsk) -> Result<bool, DomainError>; }
#[async_trait] pub trait NodeExecutor  { async fn exec(&self, node: &NodeId, call: NodeCall) -> Result<ToolResult, ToolError>; }
```

`core::ApprovalBridge` implements `ApprovalGate` (rules → card → human → expiry);
`tools::LocalExecutor` implements `NodeExecutor` for this machine, which is what
makes the brain's host a node (`nod_local`) and what a node daemon will embed in
M6. `NodeCall` is a serde-tagged enum (`{"call":"shell_exec", …}`), so the same
value that a local executor runs is the value that will cross a socket.

## 2. Domain model

Ids are `<prefix>_<uuid v7 simple>` — time-sortable, so `ORDER BY id` is honest.
Every entity is a whole-entity upsert; entries **and node roster rows** carry a
`revision`, and a stale write is a `Conflict`, never a silent overwrite. Both are
rows with more than one writer, and a whole-row upsert from a reader of an older
generation is how a concurrent write disappears.

| Entity | Type (contracts) | Id prefix | SQLite table | Written by |
| --- | --- | --- | --- | --- |
| Bot | `bot::Bot` | `bot_` | `bots` | `core::BotService` |
| Conversation (DM / group) | `conversation::Conversation` | `cnv_` | `conversations`, `conversation_members` | `BotService`, `ConversationService` |
| Transcript entry | `entry::Entry` + `EntryKind` | `ent_` | `entries` | `core::Transcript` (only) |
| Turn | `turn::Turn` | `trn_` | `turns` | `core::runners` |
| Delivery | `delivery::Delivery` | `dlv_` | `deliveries` | `Orchestrator`, `Messaging`, `BackgroundTasks`, `ApprovalService` |
| Task | `task::Task` | `tsk_` | `tasks` | `Messaging` (handoff), `BackgroundTasks` (own) |
| Task step | `task::TaskStep` | — (task + seq) | `task_steps` | `BackgroundTasks` |
| Runtime request | `request::RuntimeRequest` | `req_` | `requests` | `core::ApprovalService` |
| Approval rule | `request::ApprovalRule` | `rul_` | `approval_rules` | `ApprovalService` ("always allow") |
| Node | `node::NodeInfo` | `nod_` (`nod_local` fixed) | `nodes`, `node_keys`, `pairings` | `Swarm::ensure_local_node`, M6 gateway |
| Bot session (one mind) | `repo::BotSessionRecord` | — (keyed by bot) | `bot_sessions` | `core::Session` |
| Tool call idempotency | `repo::ToolCallRecord` | `tcl_` | `tool_calls` | `core::runners` |
| Live change | `live::LiveEvent` | — | `events_log`, `meta` (`live_epoch`) | `core::LiveHub` |
| Settings | `config::Settings` | — | `settings` (one row) | `core::SettingsHandle` |
| Invocation | `tool::Invocation` | `inv_` | *not stored* — audit JSONL on the node | `tools::AuditLog` |
| Routine / run | `routine::Routine`, `RoutineRun` | `rtn_`, `run_` | `routines`, `routine_runs` | `core::RoutineScheduler` (MIGRATION_0002) |

The port these tables sit behind is `contracts::repo::Persistence` — a bundle of
13 traits (`BotRepo`, `ConversationRepo`, `EntryRepo`, `TurnRepo`,
`DeliveryRepo`, `TaskRepo`, `RequestRepo`, `RuleRepo`, `NodeRepo`,
`SessionRepo`, `ToolCallRepo`, `EventLog`, `SettingsStore`) with a blanket impl,
so `Arc<dyn Persistence>` is the only storage type the orchestration ever names.
`SqliteStore` and `MemoryStore` implement it and are held to one shared contract
suite (`store::run_contract_suite`, 15 areas) run against both.

## 3. Lifecycle of one owner message

```
swarmbot (CLI) or swarmbot-ui ── POST /api/conversations/{id}/messages ──► brain
  └─ routes::messages::send → Orchestrator::send_owner_message(conversation, text, addressed, reply_to)
       1. reject an empty message; load the conversation; resolve reply_to → its author
       2. route()      DM    → that bot
                       group → GroupRouter::route: addressed > @everyone/@mentions
                               > reply-to author > lead  (first match wins, deduped)
       3. Transcript::owner_message  → seq from the conversation row → upsert_entry
                                       → JSONL mirror → hub.publish(Change::Entry)
       4. per target bot: Delivery::owner_message (Urgency::Urgent) → insert_delivery
          if runners.active_turn(bot) is another conversation → the owner's entry is
          re-published with post = Queued (revision + 1) so the UI can show it waiting
       5. RunnerMap::deliver(delivery) → that bot's RunnerHandle
          (no runner yet → parked in the map, drained when one registers)
```

Inside the runner (`crates/core/src/runners.rs`, one tokio task per bot):

```
run loop: next_delivery → take_batch (background deliveries of the SAME
          conversation join this turn, each attributed)
          daily token budget spent? → refuse_batch (deliveries Dropped, a task
          Failed, a routine run Failed, one warning notice) → next delivery
          → run_turn
run_turn: load bot + conversation + settings + task
          Turn::start → insert_turn → publish BotStatus::Working{activity}
          mark deliveries running (so a restart can tell them from leftovers)
execute_turn:
          Session::load                                  (one mind per bot)
          workspace <data>/workspaces/<bot name>/ (+ .spool/)
          node = store.get_node(bot.host_node())          (defaults to nod_local)
          visible = registry.visible_specs(bot.toolsets, node.capabilities(), in_task)
          render_turn_text: [Task "…"] · origin tag · group catch-up (seq > seen,
                            never its own words, ≤60 entries / 8 KiB) · deliveries
          InstructionRenderer::render(bot, {task, tools: visible, placement, …})
          backend = BackendSet::backend(bot.backend);  model = bot.model | spec.model
          context_window = spec.options.context_window | 128_000
   loop ≤ settings.max_tool_rounds:
          drain steers for THIS conversation (never inside a task) → "[Added while
          you were working] …"
          ChatRequest{model, instructions, messages: session, tools: visible,
                      extra_body, extra_headers}
          spawn stream_pump → StreamRenderer: a bot message entry is created on the
          first delta and grown in place (flush every 120 ms / 240 chars), held back
          while the text is still a prefix of NO_REPLY
          backend.chat(request, events, turn_cancel.child_token())
          merge usage → finalize_reply (empty or NO_REPLY → no entry at all)
          turn spend reached `max_tokens_per_turn`? → warning notice, keep the
          reply, stop starting rounds (the turn still completes)
          no tool calls → break (a Length finish writes a warning notice)
          else: push the assistant message, then per call run_tool_call:
                ToolCallId::from_suffix(stable_id(call.id)) → idempotency lookup in
                tool_calls → transcript tool_call entry (Running) → ToolContext →
                registry.dispatch → finish_tool_call(status, result, spill, duration)
                → push a Role::Tool message
          …a notice stops the loop after max_tool_rounds rounds
   after the loop:
          group reply → GroupRouter::wake_from_reply (minus already-addressed) →
                        hooks.wake_mentioned → Messaging (hop limit applies)
          DM reply    → forwarded once, only to a teammate who asked (a `BotMessage`)
                        and only when the text addresses them:
                        hooks.forward_dm_reply → Messaging::forward_answer, a
                        DeliveryKind::BotReply that a turn never answers (D4.8)
          NO_REPLY   → event line "@x had nothing to add"
          session.needs_compaction(context_window, compact_at_fraction) → compact
          session.save
run_turn tail: turn complete/abort/fail (+notice on failure) → update_turn →
               publish Change::Turn → deliveries marked done →
               task turn → hooks.settle_task → BotStatus::Idle → next delivery
```

Two token budgets sit at those two points, each checked where it can be honest
(D9.27). `max_tokens_per_turn` is applied **after** the round trip, because input
tokens exist only once the backend reported them: it stops the *next* round trip,
keeps the answer that crossed it, and leaves the turn `Completed` with a warning
notice. `max_tokens_per_day` is applied **before** `run_turn`, over the same
rolling 24 hours `GET /api/usage` reports by default
(`contracts::turn::DEFAULT_WINDOW_HOURS`), so a refused turn writes no `Turn` row
and never reaches a backend; `refuse_batch` drops its deliveries and settles a task
or routine run it carried as failed, since none of those rows may be left looking
as if they ran. Both numbers are what the backends reported, so a backend that
reports nothing contributes nothing — the `UsageReport` doc says so, and the
per-turn cap is a brake between round trips, not a hard ceiling.

The CLI reaches this in one of two ways, and both end at the same `Orchestrator`:
`--brain URL` drives a remote daemon over REST + `/ws/live` through
`swarmbot-client`, and no flag at all builds the same `Swarm` in process
(`cli/src/embedded.rs`). One `cli::Session` port hides the difference, so remote
output is byte-identical in shape to embedded — with two honest exceptions the port
encodes rather than papers over: `poll_interval()` is 250 ms remote versus 25 ms
embedded (a remote probe is two HTTP round trips), and `idle_confirmations()` is
**2** remote versus 1 embedded, because the brain publishes `Working` only *after*
`POST /messages` answers, so a single idle reading could end a run before the reply
it came for.

Three failure behaviours worth naming: a backend error becomes
`DomainError::BackendUnavailable` and a `This turn failed: …` notice; `FAILURE_LIMIT = 3`
consecutive failed turns put the bot in `BotStatus::Error` (the counter resets on
any success); an owner Stop
trips the turn's `CancellationToken`, kills any running tool (process group
included) and marks the turn `Aborted` with a "Stopped by the owner." notice.

## 4. Lifecycle of a handoff

```
model calls handoff_task{to, title, task, context}   (core::bot_tools, registered by Swarm::new)
  └─ Messaging::handoff_task(SendContext{from, conversation, hops}, …)
       1. blocked(): the settings.bot_messaging kill switch → Conflict
       2. resolve_bot(to)         by handle, else by label, else by raw id
       3. self-handoff refused → "use start_task for your own work"
       4. hop check chain_at(ctx.hops): at settings.max_hops → "Stopped: too many
          hand-offs (the limit is N). @x has the last word." event, Sent{stopped:true}
       5. Task::handoff(from, to, origin = the SENDER's conversation, title, brief,
          context) — built, not yet stored; requester = from
       6. event entry in the sender's conversation: Handed off "<title>" to @<name>
       7. BackgroundTasks::submit(task)   ← the one entry point both paths share
            insert_task → publish Change::Task → pump() → read the row back
            pump(): within max_running_tasks_per_bot, builds Delivery::task into the
                    OWNER BOT'S DM (bot.direct_conversation_id) and marks it Running
          → Sent{ woke: task.status == Running }
  … the recipient's turn runs in its own DM with [Task "<title>"] + the brief, and
    its tool list hides task management and the async asks (registry::TASK_TOOLS /
    ASK_TOOLS) — a group is never filled with somebody else's tool calls
  └─ run_turn tail → hooks.settle_task(task.id, final_text, failed)
       └─ BackgroundTasks::settle → succeed/fail → publish → TaskStep(Result)
            └─ deliver_result: Delivery::task_result(task.origin, recipient =
               task.requester | task.owner, title, result_text)
               → event "Task "<title>" succeeded — @lead is reading the result"
               → the requester's turn starts with "[Result of "<title>"] …"
```

**Two ways in, one lifecycle.** `start_task` (a bot's own work) and `handoff_task`
(somebody else's) both end at `BackgroundTasks::submit`, the only place a task is
inserted, published and pumped. That single funnel is what makes D4.5 hold for
both: the **work** always runs in the owner bot's DM, and only the **result**
travels — to `task.origin`, the conversation the request came from, delivered to
`task.requester` (or to the bot itself, for its own task). Because `Messaging`
hands the task over instead of delivering it, the per-bot cap, the queue order and
the delivery target cannot drift apart between the two paths.

`pump()` is a queue depth, not parallelism: `max_running_tasks_per_bot` (default 3)
caps how many of a bot's tasks are live, and a bot still runs one turn at a time
(D4.6). A bot with no runner — a disabled bot gets none, `ensure_runners` skips it
— has its delivery **parked** in `RunnerMap` until a runner registers, so nothing
is silently lost.

> **Hop accounting across a handoff.** The chain **travels with the task**
> (`Task.causal`, decision D9.1): `handoff_task` sets it from the sender's chain,
> `pump()` copies it onto the delivery it builds, and the `task-result` delivery
> copies it again — so a recipient's turn reads the real hop count and a
> `handoff_task` loop (A → B → A → …) stops at `max_hops` like any other chain.
> Work a bot takes on itself starts at `CausalChain::root()` (zero hops), because
> it is not a delegation. Without this the count restarted at zero on every handoff
> and D4.3 was silently untrue: the loop was bounded only by the task queue depth.

## 5. One mind per bot

| Rule | Where | Effect |
| --- | --- | --- |
| One session per bot | `repo::BotSessionRecord`, `core::Session` | `history` (opaque JSON array of `llm::ChatMessage`), `seen` cursors, `native_session_id`, `token_estimate` — shared by DMs, groups and tasks |
| One runner per bot, one turn at a time | `runners::BotRunner`, `RunnerMap` | two conversations can never interleave inside one model call |
| Queue, don't interrupt | `RunnerMap::deliver` / `RunnerHandle::steer` | only an *urgent* delivery for the *same* conversation is steered into the running turn; a bot message never interrupts; only the owner can stop a turn |
| A task delivery is never batched | `BotRunner::take_batch` | one turn settles exactly one task, so a delivery with a `task_id` always runs alone: folding two into one turn left the second `Running` forever, and a task stuck running occupies the per-bot cap and starves everything queued behind it (D9.2) |
| A stopped turn fails its task | `run_turn` tail → `settle_task` | an aborted turn reports "the turn running this task was stopped" as a **failure**, never an empty success — the requester has to learn the work did not finish (D9.3) |
| An answer ends the exchange | `run_turn` tail + `Messaging::forward_answer` | a DM reply is forwarded once as `DeliveryKind::BotReply`; the forwarding loop only forwards a `BotMessage` (a question), so a turn started by an answer never sends one back (D4.8) |
| Origin tags | `session::origin_tag`, `runners::render_turn_text` | `[Group chat "Team" · members: … · lead: @lead — your reply is posted here]`, `[Direct message from @ada]`, `[Answer — the exchange is over, no reply needed]` (under the `[Direct message from @ada]` tag), `[DM with the owner]` (only when the previous turn was elsewhere), `[Task "…"]`, `[An answer to what you asked]`, `[Routine run]` |
| Seen cursors | `Session::seen` / `mark_seen` | a group catch-up shows each message to each bot exactly once, never its own words, ≤60 entries and ≤8 KiB per turn |
| Compaction | `Session::needs_compaction` / `compact` | past `compact_at_fraction` (0.6) of the context window the oldest messages fold into a summary that keeps the newest `COMPACT_KEEP_LAST = 8` verbatim; the prototype's summary is head-and-tail text (`runners::compact_summary`), not a model-written one |
| New session | `Session::reset_native` | drops `history` and `native_session_id`, **keeps** `seen`: what a bot saw is a fact about the transcript, not about the model's memory |

## 6. Live hub

`core::bus::LiveHub` is the only publisher. Callers hand it a `Change`
(`Entry`, `Conversation`, `Bot`, `BotStatus`, `Turn`, `Task`, `Request`, `Node`,
`NodePresence`); the hub assigns the cursor, never the caller.

* **Commit before publish** — the row is written, then `EventLog::append_event`,
  then `broadcast`. A subscriber can always read back what it was told.
* **Cursor** — `{epoch, sequence}`; the epoch lives in the SQLite `meta` table
  (`live_epoch`, stable across restarts) and a fresh log starts at 1.
* **Resume** — `LiveCursor::can_replay_to`: same epoch and a gap ≤
  `REPLAY_WINDOW = 256` → replay from the in-memory ring (`RING_CAPACITY` is the
  same 256); otherwise the client gets `LiveEvent::Resync{reason}` followed by a
  `Snapshot`. A lagged broadcast receiver gets the same treatment, so nobody is
  ever handed a transcript with silent holes.
* **Two different buffers, two different numbers.** The *replay ring* is 256 (the
  resume promise above). The *broadcast channel* is `BROADCAST_CAPACITY = 512`
  (D9.9, raised from 64): provisioning a team publishes a bot, a conversation and
  several transcript lines within milliseconds, and a subscriber still building its
  snapshot used to lag and resync before it ever rendered anything. A lag is still
  reported honestly — this only keeps it from being the first line of every demo.
* **The HTTP forwarder adds three rules of its own** (`brain/src/live.rs`): one
  `LiveEvent` per **text** frame (never binary, so a protocol change is a decode
  error rather than a guess); end with a **1001 close frame** rather than a bare
  TCP FIN, which a client would report as `ResetWithoutClosingHandshake` where the
  truth is "the brain stopped"; and stop when either side goes away, so a leaked
  subscriber cannot keep the hub's forwarder task alive for the life of the
  process. A 30 s keepalive holds proxies open.
* **Snapshot** — `SNAPSHOT_ENTRIES = 100` newest entries per requested
  conversation, plus the whole roster, pending requests, tasks, nodes and recent
  turns; then `LiveEvent::Synchronized`.
* A subscription is filtered by conversation when the client asks for one;
  roster/node events (which have no conversation) always pass.
* `NodePresence` is deliberately not kept in the ring: the roster row that
  follows it is the replayable fact.

## 7. Storage

| Layer | What | Where |
| --- | --- | --- |
| Projection | SQLite (rusqlite, bundled), WAL, `synchronous=NORMAL`, `busy_timeout`, `foreign_keys=ON`; migrations are an ordered `MIGRATIONS` list applied under `PRAGMA user_version`, idempotent on reopen | `<data>/swarmbot.sqlite` |
| Mirror | append-only JSON Lines, one file per conversation, written after each commit; a torn last line is skipped, not fatal; `JsonlMirror::rebuild(dir, store)` replays every file into a store and returns the count | `<data>/transcripts/<cnv_…>.jsonl` |
| Audit | one JSONL line per tool invocation, written by the executing node; a failed append logs loudly and never fails the call | `<data>/audit/<node id>.jsonl` |
| Secrets | `{"ENV_NAME": "key"}`, 0600 — `brain::config` reads it and warns when the file is readable by more than its owner; the map is handed to `BackendRegistry::new`. Never in SQLite, never in argv, redacted from `Debug`, and registered with `brain::telemetry` so a stray log line is scrubbed | `<data>/secrets.json` |
| Owner auth | the one-time login token, 0600, printed once when created; `POST /api/session` exchanges it for a session cookie (a browser cannot send `Authorization` on a WebSocket upgrade). Compared in constant time | `<data>/owner-token` |
| Workspaces | one directory per bot, plus `.spool/` for spilled tool output | `<data>/workspaces/<bot name>/` |
| Backup | `store::backup` — `VACUUM INTO` (not `cp`: committed-but-uncheckpointed WAL pages are invisible to a file copy, and a torn SQLite file still opens), the transcript tree, and a `MANIFEST.json` written **last** so a half-finished backup is never mistaken for a whole one. `verify` re-derives every number from the snapshot instead of trusting the manifest | `<out>/swarmbot.sqlite`, `<out>/transcripts/`, `<out>/MANIFEST.json` |
| Restore | `store::restore` — refuses a data dir whose database another process holds open (it names the pid: a running brain keeps writing to the old inode and can overwrite the restored file), removes the stale `-wal`/`-shm` as part of the switch (a leftover WAL is replayed onto the replaced database), and takes a `VACUUM INTO` safety backup **before** touching anything, into a sibling `<data>.pre-restore-<ts>` (D9.22) | the data dir, plus that sibling |

Timestamps are stored as fixed-width RFC 3339 UTC with microsecond precision
(`store::ts`), so lexicographic order equals chronological order in SQLite and the
in-memory double sorts identically — that is what lets one contract suite compare
two implementations.

**Settings are read through, not cached** (D9.4): `SettingsHandle::get` reads the
row every time, because two holders must never disagree and a stale cache on a
policy value — the bot-messaging kill switch, a ceiling, the hop limit — is a
safety bug rather than a performance win. (The module header of `settings.rs` still
describes the old cached design; the `get` doc and the code are the truth.)

**Configured settings are seeded, and an owner's edit is never overwritten**
(D9.14): `compose::durable` writes `config.settings` into the store **only while the
store is empty**, and `compose::memory` always seeds, because an in-memory store is
always fresh. Before this, `compose::memory` used `Settings::default()`, so an
operator's `[settings]` — their safety ceilings above all — was silently ignored by
the in-memory brain (the CLI's embedded mode, `--offline`, every test). After it, a
`PATCH /api/settings` wins and a restart does not undo it; both halves are pinned by
tests.

**The two token budgets are ordinary settings, not policy-engine rules** (D9.27):
`max_tokens_per_turn` and `max_tokens_per_day` live in the same `Settings` row as
the hop limit, are read through `SettingsHandle` every turn, and a
`PATCH /api/settings` takes effect on the next turn without a restart. The per-turn
cap is checked after each model round trip; the per-day cap before a turn is
created, over the same rolling 24 hours `/api/usage` answers with when no `since=`
is given.

Their rules — including "a budget of zero is not a budget" — have **one**
implementation, `Settings::validate` in `contracts`, beside the fields it guards.
Both ways in go through it: a TOML file via `BrainConfig::validate`, and the API via
`brain::config::validate_settings`, which delegates. That is a fix rather than a
tidy-up: each path used to carry its own copy, so a file could load a cap of `0`
that the API answered `400` to, and a zero daily cap read that way refuses every
turn forever (`0 >= 0`). `a_config_file_and_the_api_refuse_the_same_settings` drives
both paths over the same bad values and fails if they ever split again.

The two caps are also described in the two front ends, from values the brain would
accept rather than from formatting invented per screen: the CLI prints one budget
line in **both** modes (`swarmbot config show` embedded and `--brain`) from the
shared `budget_line` helper in `crates/cli/src/config.rs`, and the desktop Settings
screen has two steppers ("token cap / turn", "token cap / day") built on
`stepped_token_cap`. Both print **`off`** for no cap and neither can emit `0`,
because `0` is a value the brain refuses rather than a synonym for unlimited —
pinned by `the_budget_line_says_off_rather_than_zero`,
`stepping_a_budget_never_lands_on_zero`, `a_changed_daily_budget_is_patched_alone`
and `an_unchanged_budget_is_not_patched`.

**Boot recovery** is one free function over the port
(`contracts::repo::recover(&dyn Persistence, now)`), so both stores recover
identically and one test covers both: running turns → `Interrupted`, pending
requests → `Expired` (and a request raised by an interrupted turn → `Cancelled`),
running *and* queued tasks → `Interrupted`, queued deliveries dropped.
`BackgroundTasks::recover` then re-runs interrupted tasks within
`MAX_TASK_RESTARTS = 2`; past that the origin conversation is told the work could
not be finished.

## 8. Node protocol (summary)

The brain's machine is the **local node**: `Swarm::ensure_local_node` registers
`nod_local` with the configured roots and the brain's own ceilings, and calls go
through `LocalExecutor` in process — no socket. A remote node dials **out** to
`ws://<brain>/node`, so NAT is not a problem, and the same `NodeCall` values cross
the wire. All three parts are built and tested: `crates/node-protocol` (the wire),
`crates/node` (the daemon) and `brain/src/gateway/` (the brain's half of the
socket, including the `NodeDispatcher` that implements `tools::NodeExecutor` by
routing a call to the local machine or across a socket).

Pairing and authentication in one line each (details and exact strings:
[`node-protocol.md`](node-protocol.md)):

* an 8-character Crockford-base32 code, single use, 10 minutes, stored as
  `sha256("swarmbot-pairing-v1|" + code)` registers the node's Ed25519 public key;
* every upgrade is signed over `swarmbot-node-v1|<nodeId>|<ts>|<nonce>|<origin>`,
  skew ≤ 60 s, nonces replay-cached 2 minutes;
* the first RPC is the brain's `node.hello`, whose `brainSig` the node verifies
  against the key it pinned — until then it answers everything
  `NotAuthenticated`;
* one control socket carries JSON-RPC 2.0 both ways, multiplexed by frame shape;
* policy is enforced twice, and the effective ceiling is `min(brain, node)`.

## 9. Seams for what is not built yet

Each of these is a hole with a shape, not a hole in the dark:

| Missing | The seam that is already there | Milestone |
| --- | --- | --- |
| ~~`NodeExecutor::exec` carrying the `Invocation`~~ | **Built (D9.12).** The port takes `&Invocation`, the dispatcher forwards it verbatim, and `attribute()` is gone. `unattributed()` survives only for `node.requestApproval` when a node names a bot this brain cannot place | — |
| A remote dispatcher in the embedded CLI | Not a missing wire but a property of the mode: a node dials the brain's gateway, and the embedded CLI has no listener, so nothing can pair with it. `LocalExecutor` knows its own `node_id` and **refuses** a call addressed elsewhere instead of absorbing it (D9.11 — absorbing ran a remote bot's tools on the brain's machine, invisibly). Use `--brain` when remote nodes matter; see [`node-protocol.md`](node-protocol.md) §13 item 6 | — |
| Data streams (pty, file transfer) | **Transfer built on both sides (D9.21).** Tokens, `StreamKind`, `StreamFrame`, `GET /node/stream/:id`, `StreamHub`, and the node's dialer (`node/src/stream.rs`) all exist; `fs.download` moved from the refused table to the implemented one, and `crates/node/tests/real_brain.rs` drives a real node child sending a binary file to a real brain over a loopback WebSocket. Missing: **a caller** (no route, tool or CLI command asks for a download yet) and pty (`pty.open` is still refused). See [`node-protocol.md`](node-protocol.md) §8 | M7 (in progress) |
| ~~`codex app-server` backend~~ | **Deleted (D9.19), not built.** Codex is reached as `codex-acp` over the ACP adapter — the protocol this crate already speaks, and what the shipped config uses. The variant existed only as a typed `Unavailable` that still appeared as a selectable provider, and a config naming it now fails to load instead of refusing every turn | — |
| A skill *library* UI, and per-skill enable/disable | `Bot.skills` is rendered from `<data>/skills/<name>/SKILL.md` (or `<name>.md`), capped per skill and per turn; there is no editor and no discovery, so the owner writes the files | M7 |
| ~~TLS~~ | **Built (D9.25).** `brain::tls::TlsListener` terminates TLS for the public listener over the same router and shutdown path, and the node's `--brain-ca` trusts a private CA. Proven with the real binaries (`brain/tests/tls.rs`, `node/tests/tls_brain.rs`). Not claimed: certificate hot reload, and a public-CA chain | — |
| Any non-unix host | `crates/node` calls `libc::gethostname` and `killpg` unconditionally, so the daemon is unix-only in practice | — |

## 10. Public API

`brain/src/server.rs` mounts one router on the public listener (default
`127.0.0.1:8787`). Every `/api/*` route and `/ws/live` require the owner token or a
session cookie; there is no unauthenticated owner endpoint, not even
`/api/health` (D9.10 — the UI reads the token the brain it spawned just minted,
instead of asking for an exemption). The **two node routes are the exception, by
necessity**: a node has no credential until it has paired, and its credential
afterwards is a signature over a fresh nonce, not a bearer token. What protects them
is the protocol's own order — a single-use ten-minute code for `/node/pair`, and a
`401` **before** any upgrade for `/node`
([`node-protocol.md`](node-protocol.md) §2.4, §3.1).

| Route | Purpose |
| --- | --- |
| `GET /api/health` | liveness, and what `--spawn-brain` waits for |
| `POST /api/session` | exchange the owner token for a session cookie |
| `GET`/`POST /api/bots`, `POST /api/bots/{id}/session` | the roster; New session / Compact |
| `GET /api/conversations/{id}/tail`, `POST /api/conversations/{id}/messages` | transcript paging; the owner's message in |
| `GET /api/requests`, `POST /api/requests/{id}/respond` | pending cards; Allow once / Always / Deny, and answers |
| `GET /api/tasks`, `GET /api/tasks/{id}`, `POST /api/tasks/{id}/cancel` | the delegation graph's data |
| `GET /api/nodes`, `POST /api/nodes/pairing-codes`, `DELETE /api/nodes/{id}` | the node roster, a pairing code, revocation |
| `GET`/`POST /api/rules`, `DELETE /api/rules/{id}` | approval rules |
| `GET`/`PATCH /api/settings` | the kill switch, hop limit, ceilings, and the two token caps (D9.27). A cap is a number of tokens or `null` for **no cap**; `0` is a `400 invalid_input`, not a second spelling of unlimited, and clearing one cap leaves the other alone |
| `GET /api/backends`, `GET /api/backends/probe` | the backend list (redacted) and `probe()` results |
| `GET /api/usage?since=…` | a `UsageReport`: what the window's turns spent, per bot and in total, **as the backends reported it**. Default window 24 h, `since` inclusive and RFC 3339; a malformed or future `since` is a 400, not zeros. The same rolling 24 h is the window `max_tokens_per_day` is measured over (D9.27) |
| `POST /api/shutdown` | ask the daemon to stop; the CLI's `shutdown` then polls `/api/health` for up to 10 s and says whether it actually went down |
| `GET /ws/live` | the live event socket (§6), cookie or bearer, `Origin` allow-listed |
| `POST /node/pair`, `GET /node` | the node gateway: redeem a pairing code; serve the signed control socket (§8 of this doc's sibling, [`node-protocol.md`](node-protocol.md)) |

Two rules the route layer enforces that are easy to lose: a WebSocket upgrade must
come from an allowed `Origin`, because a browser will attach the session cookie to
a page from anywhere on the internet (cross-site WebSocket hijacking); and
credentials are compared in constant time and never logged.

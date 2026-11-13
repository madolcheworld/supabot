# SwarmBot decision record

Binding decisions for this prototype, and the places where it deliberately
differs from OpenBot (`../../dbot/docs/decisions.md`), which it borrows its
shape from. Each entry says what was decided and why; where a decision is a
shortcut, it says what the real version needs.

Status: accepted for the M0–M6 prototype.

---

## D0. Product

- **D0.1 What SwarmBot is.** A self-hosted, Rust-only team of named bots that
  share one or more machines: they chat 1:1 and in groups, message each other,
  hand work off, ask for approval, and act on files and shells. Everything —
  orchestration, model calls, tools, transport, storage and the desktop UI — is
  Rust. No Node, no Bun, no TypeScript at runtime.
- **D0.2 Single owner.** One owner per brain, loopback listeners by default. No
  multi-tenancy: vendor terms forbid sharing a model subscription as a hosted
  service, and a prototype does not need it.
- **D0.3 No vendor art.** No Grok/xAI fonts, logos, icons or avatar artwork.
  Avatars are generated (a hash of the bot id → hue → tile + initial).
- **D0.4 What it is not (yet).** No computer use, no desktops/VNC, no browser
  automation, no Slack/Hark/ntfy, no routines, no skills library, no web UI.
  Each is a documented seam, not a silent omission: a node that is asked for a
  capability it does not implement answers with a typed refusal
  (`PolicyRefused { retry: false }`) rather than pretending.

## D1. Brain and hands

- **D1.1 Roles.** The **brain** (`swarmbot-brain`) runs orchestration, model
  calls, persistence and the API. **Nodes** (`swarmbot-node`) execute tools. The
  brain's machine is also a node — the *local node* — built from the same
  handler trait, in process, registered as `nod_local`.
- **D1.2 Transport.** A node dials **out** to the brain (`ws://…/node`), so NAT
  is not a problem. One control socket carries JSON-RPC 2.0 in both directions,
  multiplexed by frame shape (a frame with `method` is for your server; one with
  `result`/`error`/`chunk` is for your client). Bulk data streams get their own
  socket with a one-time token (deferred: M7).
- **D1.3 Node auth.** Ed25519. An 8-character Crockford-base32 pairing code
  (single use, 10 minutes, stored as `sha256("swarmbot-pairing-v1|" + code)`)
  registers a node's public key. Every upgrade is signed
  (`swarmbot-node-v1|<nodeId>|<ts>|<nonce>|<brainOrigin>`), the clock skew limit
  is 60 s, nonces are replay-cached for 2 minutes, and the node verifies the
  brain's signature in `node.hello` against the key it pinned at pairing. Until
  that verification succeeds the node answers everything with
  `NotAuthenticated`. Static bearer tokens are not used: a stolen URL must not be
  enough to drive somebody's machine.
- **D1.4 Policy is enforced twice.** The brain decides first (rules, cards); the
  node decides finally (roots with realpath containment, a deny list, a
  never/ask/always ceiling). The effective ceiling is
  `min(brain, node)`, so a compromised or confused brain cannot raise what a
  node will do. Every invocation is audited to JSONL on the node.
- **D1.5 Tool placement (v1 = "B1" in OpenBot terms).** The model call and the
  credentials stay on the brain. A bot's tools are our tools, aimed at its host
  node: local when the host is the brain's machine, forwarded over the control
  socket when it is not. Running a vendor CLI *on* a node (OpenBot's R1/H) is
  deliberately out of scope.

## D2. Model backends

- **D2.1 One port, several adapters.** `swarmbot_llm::LlmBackend` with
  `chat(request, events, cancel) -> ChatResponse` and `probe()`. Adapters:
  `OpenAiCompat` (SSE + native tool calls), `AcpAgent` (every Agent Client
  Protocol agent — D2.5), `Fake` (scripted, deterministic), `ClaudeCli`
  (skeleton in M3, finished with the MCP gateway), `CodexAppServer` (not
  built).
- **D2.2 Gateway quirks are configuration, not code.** A request carries
  `extra_body` and `extra_headers` that are merged verbatim, last. This exists
  because `api.inferhub.dev` auctions requests and answers a bid-less call with
  `402 no_provider_under_bid`; the bid parameter is not ours to invent, so it is
  the operator's to set. A 402 with that code is reported with the gateway's own
  minimum asks and a pointer at `extra_body`.
- **D2.3 No credential forwarding.** Keys come from the environment variable a
  backend names, or from `<data>/secrets.json` (0600). They are never written to
  SQLite, never placed on a command line, and redacted from `Debug` output and
  logs. A bot's own login (claude.ai, ChatGPT) is never read or forwarded.
- **D2.4 A harness that runs its own tool loop is honest about it.**
  `BackendKind::owns_tool_loop()` is true for the CLI harnesses: the runner then
  does not drive tool rounds, and our tools reach the model through MCP instead
  of through the response. Capabilities are declared, never faked.
- **D2.5 One ACP adapter for every Agent Client Protocol agent.**
  `kind = "acp"` spawns the configured command (`opencode acp` natively;
  Claude Code through `@zed-industries/claude-code-acp`; Codex through
  `@zed-industries/codex-acp`) and speaks JSON-RPC 2.0 over its stdio — the
  protocol is the same for all of them, so this is one adapter and not three,
  and whatever speaks ACP next is a config entry, not code. Shape: the agent
  *process* is kept alive and reused (respawned if it died; shutdown is EOF
  first — which is what makes a detached child like opencode's `serve`,
  spawned into its own process group, exit with its parent — and a `killpg`
  of the whole group after a two-second grace, which is what an `npx` wrapper
  cannot survive), but every turn gets a
  **fresh ACP session** with the bot's whole history rendered into the prompt —
  the history we send *is* the context, exactly like the stateless HTTP
  backend, and a native session on top of it would double the context.
  **The connection is not a per-turn resource.** Because every bot naming the
  backend shares one process, a per-request timeout must fail the turn that
  asked and never the shared process: a setup call (`session/new`,
  `session/set_model`, `session/set_config_option`) that runs out its budget is
  *abandoned*, not answered by a shutdown. That is safe by protocol rather than
  by hope — JSON-RPC correlates responses by id and never by order, so an
  unanswered request desyncs nothing, the client removes the pending slot with
  the dropped future, and a late answer for an id nobody waits for is logged.
  Shutdown therefore belongs to the *connection's* life (a failed handshake no
  other turn can see, `probe`'s throwaway connection, the `Drop` backstop), not
  to a turn's timeout. A **run** of unanswered setup calls is a different fact —
  the process is wedged, not the turn — so after three consecutive setup
  timeouts the next turn replaces the connection lazily, once; the old process
  is shut down only when the turns still holding it finish, so a replacement
  never kills somebody else's turn. Establishment is serialised **per
  connection key** and is cancel-aware, so one bot's cold `npx` boot cannot make
  another bot's stopped turn wait out a `setup_timeout × (max_retries + 1)`
  queue. Two limits are stated rather than hidden: there is **no idle reaper**
  — a connection lives until the backend is dropped, the agent dies, or a run of
  setup timeouts replaces it — and a finished turn releases its session with
  `session/close` only when the agent advertises
  `agentCapabilities.sessionCapabilities.close` (the method is optional in ACP
  v1, and an unadvertised optional method is not ours to call); for an agent
  that does not advertise it, its per-turn sessions live until the process
  exits. `timeout_secs` is reinterpreted as an **idle** timeout: updates reset
  it, because an agent that streams tool activity for ten minutes is working,
  not hung. Two rules keep it honest: the client declares **no** fs/terminal
  capabilities, so the agent acts through its own sandboxed tools rather than
  through hands we would not policy-check; and every
  `session/request_permission` is answered by the configured policy — default
  `reject`, `allow` is an explicit operator opt-in, and a `*_always` option is
  never chosen because a blanket grant this build does not record is D5.2's
  forbidden silent ceiling. Owner-card integration is the injected
  `AcpPermissions` implementor: the seam exists (`with_permissions`), the
  wiring does not yet. Model selection is a wish: agents that advertise the
  unstable ACP model state get `session/set_model` and a refusal is an error,
  never a silent swap; agents that do not keep their own model and say so in
  `probe`. Agents that advertise *no model list at all* (opencode's ACP server)
  can get one through `options.models_command` — the agent's own CLI as the
  lister, parsed one id per line — because "the wire is silent" must not read
  as "this agent has no models" when its CLI happily lists them. Credentials
  follow D2.3 unchanged — the child uses the owner's own
  agent login, and a resolved `api_key_env` key is injected only into the
  child's environment, never argv, logs or `Debug`.

## D3. One mind per bot

- **D3.1 One session per bot**, shared by all its conversations (DMs, groups,
  tasks). A bot that helped in a group remembers it in its DM. The cost is that
  every turn must say where its input came from; the benefit is that there is no
  "second session" confusion.
- **D3.2 One runner per bot, one turn at a time.** Deliveries queue FIFO across
  conversations. A turn belongs to exactly one conversation, and its replies,
  tool calls, cards and usage all land there.
- **D3.3 Queue, don't interrupt.** Only an *urgent* delivery for the *same*
  conversation is steered into a running turn. A bot message never interrupts a
  turn; only the owner can stop one. Background deliveries of the same
  conversation are batched into the next turn, each attributed.
- **D3.4 `NO_REPLY` is silence, not a message.** A group reply that is exactly
  `NO_REPLY` (trimmed, case-insensitive) is not posted; an event line records
  that the bot had nothing to add. Streaming holds back a reply while the text
  is still a prefix of `NO_REPLY`, so the token never flashes into the
  transcript. Anything longer than the token is posted: swallowing a real answer
  is worse than posting a redundant one.
- **D3.5 Compaction instead of overflow.** When a session passes
  `compact_at_fraction` of the backend's context window, the oldest messages are
  folded into a summary that keeps the newest eight verbatim. The prototype's
  summary is head-and-tail text, not a model-written one — it says
  "(N earlier lines folded away)" rather than pretending to understand them.

## D4. Delegation

- **D4.1 Delegation is tools, not machinery.** `message_bot`, `post_to_group`,
  `handoff_task`, `start_task`, `react`, `ask_user`, `request_approval`. A model
  that can call a tool can delegate; there is no separate planner.
- **D4.2 Group work stays in its group.** From a group turn, a message to a
  fellow member is posted *in the group* as `@name …`, not into a private DM. A
  team that splits into side channels is a team the owner cannot follow.
  `message_bot` is for what none of your groups is about.
- **D4.3 Hop limit 8 per causal chain**, configurable, plus a global kill switch
  (`settings.bot_messaging`). Reaching it writes "Stopped: too many hand-offs"
  into the transcript rather than silently dropping the message.
- **D4.4 A mention wakes a member once per reply**, with the final message rather
  than mid-turn commentary, and never somebody who was already addressed.
- **D4.5 Task results travel back to the origin.** A task's *work* happens in the
  owner bot's DM (so a group is not filled with somebody's tool calls); its
  *result* is delivered to the conversation it came from, to the bot that asked.
- **D4.6 Deviation, on purpose: tasks share the bot's session.** OpenBot gives a
  task its own ephemeral session; here a task is a tagged turn in the bot's one
  session, and `max_running_tasks_per_bot` is a queue depth rather than true
  parallelism. `Task.session_key` is kept so a separate session can be added
  without changing a task's shape.
- **D4.7 Deviation: `react` writes an event line.** Entries are whole-entity
  upserts, and a reaction would need its own schema; a line
  ("@scout reacted 👍 to the owner's message") is honest and searchable.

## D5. Approvals and safety

- **D5.1 Rules first, a human second, default ask.** Order: bot deny > bot ask >
  bot allow > global deny > global ask > global allow > ask. A default that runs
  is a default that surprises.
- **D5.2 "Always allow" writes a rule as narrow as the request allows**: the bot,
  the surface, and for a shell command the first word. One yes does not become a
  blanket one.
- **D5.3 Long waits never block a tool call.** `ask_user` and
  `request_approval` return `{status: pending, id}` immediately; the answer (or
  the expiry) arrives as a delivery that wakes the bot. A blocking ask — a tool
  that cannot continue without a yes — waits on a card and expires after
  `approval_expiry_secs` (10 minutes by default).
- **D5.4 Cards sit next to the work.** A request raised during a group turn is
  shown in the group, not in the bot's DM: splitting the conversation hides the
  reason a bot stopped.
- **D5.5 Destructive commands always ask.** Even with a `shell = always`
  ceiling, a command matching the destructive list (`rm -rf`, `sudo`, `mkfs`,
  `dd if=`, `git reset --hard`, `curl … | sh`, …) is at least an ask. The matcher
  blanks quoted spans so `echo "rm -rf"` is not flagged, and inlines `sh -c '…'`
  payloads so a wrapper is not a loophole; it does not parse shell, and says so.
- **D5.6 Process groups, always.** A shell tool call runs with `process_group(0)`
  and the whole group is killed on timeout, cancellation or a dropped socket. A
  surviving child that nobody owns is worse than a killed one.
- **D5.7 Environments are scrubbed.** A child gets `PATH`, `HOME`, `LANG`,
  `LC_ALL`, `TERM`, `TMPDIR` and whatever the caller passed explicitly — never
  the brain's environment, which may hold API keys.

## D6. Storage and liveness

- **D6.1 SQLite is the projection, JSONL is the mirror.** rusqlite (bundled) in
  WAL mode, migrations by `user_version`; every committed entry is also appended
  to `<data>/transcripts/<conversation>.jsonl`. A lost database can be rebuilt
  from the mirror (`JsonlMirror::rebuild`), and a torn last line is skipped
  rather than fatal.
- **D6.2 Whole-entry upserts with a revision guard.** An entry is replaced as a
  whole and its `revision` must increase; a stale write is a `Conflict`, not a
  silent overwrite. This is what lets a streaming reply grow in place. The same
  guard now covers node roster rows (D9.18): a whole-row upsert with several
  concurrent writers is exactly the shape that loses updates, and "it has always
  worked" was only true until two writers overlapped.
- **D6.3 Commit before publish.** A change is written, then appended to the event
  log, then broadcast. A subscriber can always read back what it was told.
- **D6.4 A cursor is a promise.** Live clients resume with `{epoch, sequence}`; a
  gap of 256 or fewer is replayed, anything larger (or a new epoch) gets a
  snapshot. A client is never given a transcript with silent holes.
- **D6.5 Recovery is honest.** At boot, running turns become `interrupted`,
  pending requests `expired`, running and queued tasks `interrupted`, and queued
  deliveries are dropped. Tasks re-run within a budget of two restarts; past
  that the origin conversation is told the work could not be finished.
- **D6.6 Unknown enum variants decode.** Transcript entries carry an
  `#[serde(other)] Unknown` so a client built against an older schema renders a
  placeholder instead of failing.

## D7. Interfaces

- **D7.1 One API, three clients.** The brain serves REST + a live WebSocket; the
  CLI and the GPUI app both speak it through `swarmbot-client` (blocking, no
  tokio). Nothing else may embed the orchestration.
- **D7.2 The UI is a separate process.** GPUI owns the main thread and its own
  executor; tokio owns the brain. They meet over loopback HTTP, so a UI can also
  point at a brain on another machine. `--spawn-brain` supervises the child for
  a one-command start.
- **D7.3 The CLI can run embedded** (`swarmbot run --embedded`) for a
  zero-setup single process; that is a convenience, not a second code path — it
  builds the same `Swarm`.
- **D7.4 Instructions are rendered from the store.** A prompt never lists a tool
  the bot does not have: the roster, the groups and the tool list come from what
  is actually configured, so a capability change is reflected on the next turn.

## D8. Engineering

- **D8.1 Crate boundaries are dependency rules.** `contracts` knows nothing;
  `llm`/`tools`/`store`/`node-protocol` know `contracts`; `core` knows those;
  `brain`/`node`/`cli`/`ui` compose. `core` never opens a socket; `ui` never
  touches `core` or `store`.
- **D8.2 Everything is testable offline.** The `Fake` backend is scripted, the
  store has an in-memory double that shares one contract suite with SQLite, and
  no test touches the network. Live tests are `#[ignore]`d behind
  `PROTO_LIVE_LLM` / `PROTO_LIVE_CLAUDE` and stay trivially cheap.
- **D8.3 Cargo home lives in the checkout** (`.cargo-home`, seeded by reflink
  from `~/.cargo`) so builds work under a sandbox that cannot write to the user's
  home. `scripts/env.sh` exports it together with `LIBCLANG_PATH` (gpui's
  `bindgen`) and `VK_ICD_FILENAMES` (llvmpipe).
- **D8.4 gpui is pinned exactly** (`=0.2.2`) and built with
  `default-features = false, features = ["font-kit", "wayland"]`: it is pre-1.0
  with frequent breaking changes, and this machine has no `xkbcommon-x11` dev
  files for the `x11` feature.
- **D8.5 Rust edition 2024, stable toolchain**, `clippy -D warnings`, no
  `unwrap()` outside tests. A panic in a runner would take a bot's mind with it.

---

## D9. Decisions taken while building (added after D0–D8)

These were discovered by tests or by an audit, not chosen up front. Each one
names the failure it prevents, because the code reads as ordinary until you know
what it is holding back.

- **D9.1 A task carries its causal chain.** `Task.causal: CausalChain` and the
  task-result delivery inherits it. Without this, a `handoff_task` loop
  (A → B → A → …) restarted the hop count at zero on every hop and was bounded
  only by the task queue depth — D4.3 was silently untrue. `CausalChain::root()`
  is the honest start for work a bot takes on itself.
- **D9.2 A task delivery is never batched.** One turn settles exactly one task,
  so folding two task deliveries into a turn left the second `Running` forever;
  a task stuck running occupies the per-bot cap and starves everything queued
  behind it. `BotRunner::take_batch` skips any delivery with a `task_id`.
- **D9.3 A stopped turn fails its task.** An aborted turn reports
  "the turn running this task was stopped" as a failure, never an empty success:
  the requester has to learn the work did not finish.
- **D9.4 Settings are read through, not cached per handle.** Two
  `SettingsHandle`s must never disagree, and a stale cache on a policy value —
  the bot-messaging kill switch, a ceiling, the hop limit — is a safety bug, not
  a performance win. One row read is cheap.
- **D9.5 A `Peer` holds its driver weakly.** `Driver` keeps a
  `WeakUnboundedSender`, so dropping every handle really does close the
  connection (`Exit::HandlesGone` is reachable). It used to hold a strong clone,
  which made that branch dead code and leaked one task and one socket per
  disconnected node. `close(code, reason)` is still how you send a close code;
  drop is the leak-proof fallback, not the polite goodbye.
- **D9.6 A secret is a type, not a `String`.** `ui::proto::SecretToken` (and the
  hand-written redacting `Debug`s in `llm`, `client` and `brain`) exist because
  `#[derive(Debug)]` on a struct that holds a token is one log line away from
  leaking it. The value is unwrapped only at the wire call; every test asserts
  the bytes appear in no `{:?}` and no report.
- **D9.7 `fs.*` results cross the wire as `contracts::ToolResult`.** Not the
  `rpc::Fs*Result` structs of the upstream doc: those cannot be built from a
  `LocalExecutor` result without writing every fs handler a second time, and two
  implementations of `read_file` is exactly the drift this design avoids.
  Consequence, stated plainly: `fs.mkdir`/`fs.move`/`fs.remove` return a
  `ToolResult`, not `null`.
- **D9.8 A condition that cannot cross the wire is refused, not dropped.**
  `rpc::FsEdit` carries `expectedSha256` but `NodeCall::FsEdit` does not, so both
  the node and the brain answer `PolicyRefused { retry: false }` and say why.
  Silently dropping a hash check turns a safety check into a clobber.
  `fs.write` conditionals *do* cross, and are enforced.
- **D9.9 The live hub's ring is 512, not 64.** Provisioning a team publishes a
  bot, a conversation and transcript lines within milliseconds; a subscriber
  still building its snapshot lagged and resynced before it ever rendered
  anything. A lag is still reported honestly (`Resync` + a fresh snapshot) — this
  only keeps it from being the first line of every demo.
- **D9.10 `--offline` authenticates itself.** The UI reads the `owner-token` the
  brain it spawned just minted, in a bounded retry, rather than asking the brain
  to exempt `/api/health`. No endpoint is unauthenticated; the demo needs no
  credentials either.
- **D9.11 An executor knows which node it is.** `LocalExecutor` gained a
  `node_id` (default `nod_local`, set with `for_node`) and **refuses** a call
  addressed to any other node instead of logging a mismatch and running it. It
  used to absorb those calls, which meant a bot bound to a remote node had its
  tools executed on the brain's machine, inside the brain's roots and under the
  brain's ceilings — invisible in the transcript, because the tool-call entry
  still named the node the model asked for. A node daemon passes its own paired
  id, so calls addressed to itself are its own to run. Consequence, and it is a
  property of the mode rather than a missing wire: the embedded CLI (`swarmbot
  run`, which composes through `core::compose`) has no remote dispatcher, so a
  remote-bound bot there gets a clear typed refusal. Putting a dispatcher in
  `core::compose` would not help — a node dials the brain's gateway, and the
  embedded CLI has no listener, so nothing could pair with it and the dispatcher
  could only ever answer "not connected". Remote nodes are the daemon's feature;
  use `--brain` when they matter.
- **D9.12 `NodeExecutor::exec` should carry the invocation.** Every `fs.*` and
  `shell.exec` payload needs `inv: Invocation`, and a node echoes `inv.bot_id`
  back in `node.requestApproval` — but the port is `exec(&self, node, call)`, so
  the brain's dispatcher has to *derive* attribution: exactly one running turn
  belonging to a bot hosted on that node ⇒ that turn; otherwise a `bot_unknown`
  sentinel, never a guess. The derivation is sound today (a tool call only
  happens inside a turn, and a bot runs one turn at a time), but it is inference
  where the data was available at the call site. Widening the port to
  `exec(&self, node, inv, call)` — or putting the invocation inside `NodeCall` —
  deletes the sentinel and the dedup window. **Done**: the port is
  `exec(&self, node, &Invocation, call)`, `NodeDispatcher::attribute()` and its
  `running_turns()` scan are gone, and the dispatch path has no fallback that
  invents attribution. `unattributed()` survives only for the
  `node.requestApproval` path, where a node names a bot this brain cannot place.
  The change touched `tools`, `core`, `node` and `brain` at once, as predicted;
  `two_turns_on_one_node_each_carry_their_own_invocation_to_the_node` pins it and
  was mutation-checked against the old give-up path.
- **D9.13 An empty test is a lie, so there are none.** A test whose body is
  empty passes trivially and reads as coverage. The `#[ignore]`d
  `pair_and_serve_against_a_real_brain` recipe in `crates/node` was deleted and
  replaced with a comment naming where each of its six steps *is* tested
  (`brain/tests/node_gateway.rs`, `target/two-process/run.sh`, this crate's
  `pairing.rs`/`hello_gate.rs`/`redial.rs`) plus what none of them cover: a
  remote tool call in the two-process run, and `wss://`/TLS.
- **D9.14 Configured settings are seeded, and an owner's edit is never
  overwritten.** `compose::memory` used `Settings::default()`, so an operator's
  `[settings]` — their safety ceilings above all — was silently ignored by the
  in-memory brain (the CLI's embedded mode, `--offline`, and every test).
  `compose::durable` now seeds `config.settings` **only while the store is
  empty**, and `memory` always seeds, because an in-memory store is always
  fresh. After that a `PATCH /api/settings` wins and a restart must not undo it:
  both halves are pinned by tests.
- **D9.15 A scripted bot gets its own script queue.** `--script` used to push
  every bot's turns into the one shared `fake` backend, so the queue was global:
  under CPU contention a bot woken mid-delegation could pop a turn written for
  another bot. It failed **7 times in 25** multi-threaded runs of the suite, and
  it failed *silently* — the wrong scripted reply, in the wrong conversation,
  with exit code 0. Loading a script now synthesises one fake backend per
  scripted bot (`fake-lead`, `fake-scout`, …, the `demo` idiom) in the
  config-resolution path, before the swarm is composed. Only `fake` backends
  split; a real model is never replaced by a script. After the fix: 0 failures in
  95 multi-threaded runs. The first attempt at this "fixed" it by pinning the test
  to a current-thread runtime, which is the wrong half of the problem — a test
  that cannot fail is not evidence. Hence the rule: when a test hides a race, fix
  the product and put the test back on the multi-threaded runtime.
- **D9.16 The usage window rule lives in `contracts`, not in the route.** Two
  front ends answer "what did this team spend": the brain at `GET /api/usage` and
  the CLI's embedded mode reading its own store. Written twice, the rules that are
  easy to get subtly wrong — `since` **inclusive**, `until` enforced as a real
  upper bound *after* the read, a deleted bot keeping an unnamed row, a store
  error reading a bot never failing the report — would drift, and the one that
  drifted would be wrong in a way nobody notices, because a report is only ever
  compared against a number somebody remembers. `UsageReport::window` is that one
  function; both callers go through it. Two things the report deliberately does
  **not** do: estimate (a backend that reports nothing leaves zeros, and `turns`
  is carried so "nothing ran" is distinguishable from "turns ran, nothing
  reported"), and price (converting tokens to money needs the owner's per-model
  rates; a built-in table would be a guess with a decimal point).
- **D9.17 A backup is `VACUUM INTO`, and its manifest is verified against the
  snapshot.** Copying `<data>/swarmbot.sqlite` while the brain runs is not a
  backup: committed transactions may still be in the `-wal`, so the copy is stale,
  and a torn SQLite file still opens and reports plausible data — the failure is
  silent, which is the worst kind for the one file holding everything. The
  snapshot therefore goes through SQLite's own locking, and `MANIFEST.json` is
  written **last** (via temp + rename) so a half-finished backup leaves a
  directory that `verify` refuses rather than one that merely looks complete.
  `verify` re-derives every number from the snapshot and the transcript tree
  instead of believing the manifest: a manifest that lies is a detected failure.
  Restoring is deliberately not a code path — it is "open the snapshot as a
  store", which `SqliteStore::open` already does, and the test suite proves that
  rather than trusting the bytes. A non-empty target is a typed refusal, never a
  silent overwrite.
- **D9.18 The node roster row is revision-guarded, and a hello cannot undo a
  revocation.** Several writers share one row — `node.hello`, `node.heartbeat`,
  `node.event`, presence changes and revocation — and the mux spawns a task per
  inbound request, so they overlap in practice, not in theory. Each used to be a
  hand-rolled `get_node` → mutate → `update_node`, and `update_node` writes the
  whole row: a heartbeat that read before the hello wrote restored the pre-hello
  row, reverting the node's `policy` while it was visibly online, and the brain
  then planned auto-runs against a ceiling the node did not have. `NodeInfo` now
  carries a `revision` (D6.2's invariant), both stores answer `Conflict` on a
  stale write, and every gateway writer goes through `session::mutate_node`,
  which re-reads and re-applies on conflict up to a bound.
  The second half is a security rule, not a concurrency one: **`apply_hello` must
  never clear `revoked`.** It used to, so a hello already in flight when the owner
  revoked the node resurrected it — and `NodeDispatcher` routes on that row, so
  the brain would send tool calls to a machine its owner had just cut off. The
  revoked check lives *inside* the CAS-guarded loop (`mutate_node_vetoed`), not
  before or after it: a check placed before has the same window the guard closes,
  and a check placed after has already written `online: true` and re-approved the
  node's capabilities. A vetoed hello writes nothing and the socket closes 4401.
  Only re-pairing, which writes a fresh row, may clear a revocation.

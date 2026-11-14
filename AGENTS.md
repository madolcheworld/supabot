# Working in SwarmBot

Rules for anybody continuing this repository — human or agent. They exist because
several people (and several agents) build this workspace at the same time, and
because a bot that lies about what it did is worse than a bot that does less.

**Read first, in this order:**

1. [`docs/decisions.md`](docs/decisions.md) — binding. Where this file and the
   decision record disagree, the decision record wins.
2. [`docs/architecture.md`](docs/architecture.md) — crate graph, domain model, the
   lifecycle of a message and of a handoff, and the seam each unbuilt feature
   hangs off.
3. The doc for the layer you are touching:
   [`docs/tools.md`](docs/tools.md), [`docs/node-protocol.md`](docs/node-protocol.md),
   [`docs/ui.md`](docs/ui.md).
4. The module docs of the crate you are in. They state the invariants; the code
   assumes you read them.

---

## Environment

```bash
source scripts/env.sh      # before ANY cargo command, in every new shell
scripts/check.sh           # the gate: fmt + clippy -D warnings + test
scripts/check.sh --fast    # the same, without swarmbot-ui (its build is minutes long)
```

`scripts/env.sh` is not optional and not cosmetic: it points `CARGO_HOME` at
`.cargo-home` inside the checkout (a sandbox cannot write to `~/.cargo`), sets
`LIBCLANG_PATH` (gpui's `bindgen`; there is `libclang.so.22.1` but no unversioned
symlink), builds the `libxkbcommon-x11.so` link shim and exports `LIBRARY_PATH`,
and pins `VK_ICD_FILENAMES` to llvmpipe **when that ICD exists**. Without it a
`cargo build -p swarmbot-ui` fails in three different ways.

**The GPU works again, as re-checked on 2026-11-14.** The NVIDIA driver is up
(`/dev/nvidia0`, `/dev/nvidiactl`, `/dev/nvidia-modeset`, `/dev/nvidia-uvm`,
`/dev/dri/card1`, `/dev/dri/renderD128`; `nvidia-smi` reports a GTX 1660 SUPER on
driver 615.78.08) and `nvidia_icd.json` is the ICD in use. `swarmbot-ui
--selftest` therefore opens **11 windows** and exits 0 — the window count is
deterministic and is the number to assert on. The **total render count is not**:
it was 22, 54, 55, 57 and 58 across five runs of the same binary, because it
counts frames and frames depend on timing. Quote the window count, or say
"renders"; do not quote a render total as if it were a fact. The
2026-11-13 note that stood here ("llvmpipe ICD missing, `NoSupportedDeviceFound`
panic") described that day, not this one — do not resurrect it without running
`./target/debug/swarmbot-ui --selftest` first. `scripts/env.sh` still prefers
llvmpipe **when that ICD exists** and still says so on stderr when no ICD is
found; that fallback is for headless boxes, not a claim about this one.

What is *still* not proven is the pixels: `--selftest` proves a window opened and
the root view rendered, not that anything looked right. On 2026-11-14 the desktop
session was locked, so a screenshot showed the lock screen — say "renders", not
"looks correct", unless somebody actually looked at an unlocked screen.

Cargo builds share `./target`. `Blocking waiting for file lock on build directory`
is other people working, not a hang: wait it out. Do not invent a second target
directory — it recompiles every shared dependency.

This checkout **is** a git repository. Do not commit, do not tag, and do not
rewrite history unless the owner asks.

## Crate ownership during parallel work

* Write **only** inside your own crate directory (its `Cargo.toml` included).
* **Never** edit the root `Cargo.toml`, `Cargo.lock`, another crate, or another
  crate's docs. If you need a dependency that is not already in
  `[workspace.dependencies]`, do not add it: implement without it and report the
  missing dependency in your summary. A lock rewrite collides with every other
  agent mid-build.
* The same applies to test-only dependencies. `tempfile` is already in the lock
  file and used by `store`, `tools` and `core`; if your crate cannot have it,
  hand-roll a temp dir (see `crates/cli/src/tmp.rs` for why).
* Cross-crate contracts live in `swarmbot-contracts`. If you need a new field or a
  new port method, that is a change to `contracts` and it is the workspace owner's
  to make — propose it, do not edit it from inside an adapter crate.

## Code rules

* Edition 2024, stable toolchain (`rust-toolchain.toml`), `cargo fmt`, and
  `clippy --all-targets -- -D warnings` clean.
* **No `unwrap()`, `expect()`, `panic!`, `todo!` or `unreachable!` outside
  `#[cfg(test)]` and test-support modules.** Two accepted exceptions: an `expect`
  whose message names an invariant that cannot fail (`expect("index in range")`),
  and a poisoned mutex, which is recovered with
  `lock().unwrap_or_else(|e| e.into_inner())`, never with a bare `unwrap()`. A
  panic inside a runner takes a bot's mind with it.
* **Errors are values.** `DomainError` (domain), `ToolError` (tools),
  `LlmError` (backends), `ClientError` (client). Never a stringly-typed panic, and
  never a `Result` whose error is `Box<dyn Error>` at a port boundary.
* **Dependency direction is a rule, not a preference:**
  `contracts` → {`llm`, `tools`, `store`, `node-protocol`} → `core` →
  {`brain`, `node`, `cli`, `ui`}. `core` never opens a socket, reads a config file
  or knows HTTP; `ui` never touches `core` or `store`; `tools` never imports
  `core` (it reaches a human through `ApprovalGate` and a machine through
  `NodeExecutor`).
* Doc comments explain **why** and name the invariant. A comment that restates the
  next line is noise; a comment that says "a stale write must be a conflict, not a
  silent overwrite" is the reason the code looks the way it does.
* Keep the seams: a new capability that belongs behind a port gets a port. Do not
  wire a service directly into a crate that the dependency rules put above it.

## Tests

* **No test touches the network.** Live tests are `#[ignore]`d and gated:
  `PROTO_LIVE_LLM=1` (plus `PROTO_LIVE_BASE_URL`, `PROTO_LIVE_API_KEY`,
  `PROTO_LIVE_MODEL`) and `PROTO_LIVE_ACP=1` (plus `PROTO_LIVE_ACP_CMD`/`_ARGS`;
  free handshake unless `PROTO_LIVE_ACP_PROMPT=1`). There is **no**
  `PROTO_LIVE_CLAUDE` test, whatever older text said — `ClaudeCli` is covered
  offline against a stand-in binary, and its real path is the same ACP/CLI shape
  this list already names. `scripts/opencode-live.sh [backend-id]` is the one
  command that runs the ACP gate against the box's own agent (opencode by
  default), and it is the honest answer to "can this machine complete a turn?".
  They stay trivially cheap — one turn — and they print what they spent.
  Three exist: `llm`'s `live_gateway_round_trip` (one turn, also asserts the
  adapter filled `Usage`), `llm`'s `live_acp_agent_handshake_and_one_cheap_turn`
  (the one `scripts/opencode-live.sh` runs; without `PROTO_LIVE_ACP_PROMPT=1` it
  stops at the free handshake), and `node`'s `live_two_process` — two real
  processes and a real model, which is the only proof of a remote tool call in
  that shape (D9.26). The last one needs **both** binaries built first
  (`cargo build -p swarmbot-brain -p swarmbot-node`), because
  `CARGO_BIN_EXE_<name>` only exists for a package's own bins.
* **No test writes outside a `TempDir`**, and no test runs a destructive command
  (`rm -rf`, `sudo`, `mkfs`, `dd`, `git reset --hard`, …). A `shell` test must kill
  its own process group and assert that it is gone.
* Orchestration tests run on `MemoryStore` + `FakeBackend`. That is the real code
  path — only the store and the model are doubles — so a test that needs a real
  database or a real model is testing the wrong layer.
* **Storage changes go into the contract suite.** `store::run_contract_suite` runs
  one body of assertions against `SqliteStore` and `MemoryStore`; add your
  assertion there, not to one implementation's own tests. The two doubles must stay
  observably identical (same ordering, same conflict messages, same counts).
* Time-dependent behaviour uses `tokio::time::pause` / `TestClock`-style control,
  never a sleep long enough to be flaky.
* Wire work is tested without sockets: `node_protocol::testing` is public test
  support (`LineSocket`, `duplex_pair`, `ScriptSocket` + `SentLog`), and
  `mux::Socket` is three methods, so a foreign transport is a small impl. Use it
  rather than binding a port in a unit test.
* A behaviour change needs a test that fails without it. A bug fix needs the test
  that reproduces it.

## Honesty rules (binding)

* **Never fake a capability, a delegation, a result, an approval or a permission
  mode.** If something is not implemented, return a typed refusal —
  `ToolError::PolicyRefused { retry: false }`, `LlmError::Unavailable { reason }`,
  a `NotImplemented`-shaped `PolicyRefused` on the node wire — and say what is
  missing. A stub that returns success teaches everybody downstream to trust a lie.
* **Never silently enable a bypass** (a `--dangerously-skip-permissions`-style
  flag, an auto-allow ceiling), and never raise a node's ceiling from the brain:
  the effective ceiling is `min(brain, node)`.
* **Never forward credentials.** No claude.ai or ChatGPT token is read, copied or
  sent anywhere. API keys come from `api_key_env` or `<data>/secrets.json` (0600),
  live only inside an adapter's private field, and appear in no `Debug` impl, log
  line, error message, SQLite row or argv. If you add a type that could hold one,
  implement `Debug` by hand and add the redaction test.
* **A denial is data, a failure is an error.** `Ok(ToolResult::error("denied by the
  owner: …"))` is a legitimate answer a model can react to; `Err` means
  infrastructure broke. Do not mix them.
* **State limits plainly.** The destructive-command matcher is not a shell parser
  and its doc comment says so; the compaction summary is head-and-tail text, not a
  model-written one; a `read_file` of a binary says it is not text. Follow that
  example in every error message and doc you write.
* **The audit log never fails a tool call** — but a failed append is logged loudly,
  because a silently missing line is how an audit log stops being one.
* When you cannot verify something, say so in your report instead of describing
  what you intended to happen.

## Where things are

| Crate | Owns | Start here |
| --- | --- | --- |
| `contracts` | every domain type, the repository ports, the live-event shapes | `src/lib.rs` (module map), `src/repo.rs` (the ports) |
| `llm` | `LlmBackend` + adapters (OpenAI-compatible, **ACP agents** — opencode/Claude Code/Codex over the Agent Client Protocol, Fake, Claude CLI skeleton), `BackendRegistry` | `src/backend.rs`, `src/openai.rs`, `src/acp/mod.rs` |
| `tools` | `ToolRegistry`, the seven handlers, `PathPolicy`, `PolicyEngine`, `AuditLog`, `LocalExecutor` | `src/lib.rs`, `src/handler.rs` (the ports) |
| `store` | `SqliteStore`, `MemoryStore`, `JsonlMirror`, the shared contract suite, backup/restore, and `AsyncStore` (the async facade) | `src/lib.rs`, `src/contract.rs`, `src/async_store.rs` |
| `core` | runners, orchestrator, routing, messaging, tasks, approvals, transcript, live hub, instructions, composition | `src/lib.rs`, `src/runners.rs`, `src/swarm.rs` |
| `client` | the blocking REST + live-WS client shared by CLI and UI | `src/lib.rs` |
| `brain` | composition root: axum REST (plaintext or TLS), `/ws/live`, owner auth, telemetry, and the node gateway (`/node/pair`, the signed `/node` upgrade, `NodeDispatcher`) | `src/lib.rs`, `src/server.rs` (the route table), `src/routes/`, `src/gateway/` |
| `cli` | `swarmbot`: demo, run, chat, bots, transcript, tasks, nodes, config, shutdown — embedded *and* `--brain` | `src/main.rs` (commands), `src/session.rs` (the `Session` port both modes implement), `src/remote.rs` |
| `ui` | `swarmbot-ui`: the GPUI desktop app | `src/lib.rs` (module map), `src/reducer.rs` (where the correctness lives), [`docs/ui.md`](docs/ui.md) |
| `node-protocol` | the wire: mux, pairing, signed upgrade, typed errors, stream tokens **and framing**, the shared rustls provider helper | `src/lib.rs` (module map), [`docs/node-protocol.md`](docs/node-protocol.md) |
| `node` | `swarmbot-node`: the daemon that executes under its own policy, streams a file over a data stream, and speaks `wss://` | `src/lib.rs`, `src/handlers.rs` (the `rpc` ↔ `NodeCall` bridge), `src/shell.rs`, `src/stream.rs`, `tests/real_brain.rs` (the end-to-end proof) |
| `mcp` | the tool registry as an MCP server (stdio + streamable HTTP) | `src/lib.rs`, `src/transport.rs` |

## Known debt

Recorded on purpose, so nobody "fixes" them by accident or trusts them further
than they go. This list carries two kinds of entry, and says which is which: what
is **still open**, and what was **closed in a way worth remembering** — because a
closed bug whose shape is not written down gets reintroduced by somebody who has
never seen it fail. Two further closed entries (the `exec_shell` mirror and the
node's out-of-root spill dir) live in
[`node-protocol.md`](docs/node-protocol.md) §13 items 1–2 with the tests that pin
them.

* **One CLI test is suspected flaky under port reuse.** A single gate run showed
  `105 passed; 1 failed`; the failing name was not captured and it never
  reproduced in 30 subsequent runs (8 of them under simulated 4-core load). The
  suspects are the pre-existing loopback-port tests in `crates/cli/src/remote.rs`
  (e.g. `a_refused_connection_names_the_url_and_the_likely_cause`), which bind
  `127.0.0.1:0` and can race with another process grabbing the port between bind
  and connect — a known TOCTOU shape, made more likely when several agents share
  this machine and `./target`. If you see a one-off CLI failure, re-run before
  you debug; if it reproduces, fix it by holding the listener for the duration of
  the assertion rather than by adding a sleep.
* **A `real_brain` failure was diagnosed, and it was not a load problem.**
  `revoking_closes_the_real_child_with_4401_and_a_redial_is_refused` used to fail
  with "the refusal is a dial failure the node reports honestly:
  `2026-11-13T01:33:44.929401Z  INFO swarmbot-node starting …`". The needle was
  `401` and the **tracing timestamp** contains `401` (`.929401`), so the helper
  answered with the daemon's startup line before the refusal ever appeared. More
  log lines — a busier machine — meant more chances for the clock to answer the
  question, which is why it looked like load sensitivity. `wait_for_line` now
  matches `message_of(line)` (the line with its tracing prefix stripped) and
  `a_timestamp_is_never_mistaken_for_the_message` pins it. The lesson generalises:
  **when a helper matches a short token against a log line, strip the prefix
  first.** A remaining `real_brain` failure is a real failure; do not call it a
  flake without the log line in hand.
* **Four bugs shared one symptom: "`real_brain` is flaky under load."** All four
  are fixed; they are listed here because the shape recurs and the fix for each
  was different. (1) The roster row had several concurrent writers doing
  read–modify–write against a whole-row upsert, so a heartbeat could restore the
  pre-hello row and revert a node's `policy`. `NodeInfo` now carries a
  `revision`, `update_node` answers `Conflict` on a stale write in both stores,
  and every gateway writer goes through `session::mutate_node`, which re-reads
  and re-applies. **If you add a roster writer, use `mutate_node`.**
  (2) `Swarm::wait_for_idle` trusted only in-memory signals, which all read quiet
  in the window at the start of a turn — it now also believes
  `store.running_turns()`. This was not only a test problem: `swarmbot run --wait`
  uses the same function and could exit 0 claiming a finished team.
  (3) A panicking runner cleared its `busy` flag but left its turn `Running`
  forever; it is now closed as `Failed` naming the panic. (4) `apply_hello`
  assigned `revoked = false`, so a hello in flight when the owner revoked
  resurrected the node — and the dispatcher routes on that row. The revoked check
  now lives *inside* the CAS-guarded loop, so there is no window: a conflicting
  write re-reads, the veto fires, nothing is stored, and the socket closes 4401.
  The lesson that generalises: **make the failure print the state, not just the
  expectation.** One `turn_report()` in an assertion message turned "the
  transcript is empty" into "the turn is still Running with 0 LLM calls", which
  is what identified (2). Tests: the lost-update interleaving in `store`'s
  contract suite, `a_conflicting_roster_write_is_retried_and_lands`,
  `a_turn_the_store_calls_running_is_not_idle`, and
  `a_hello_for_a_revoked_node_does_not_unrevoke_it` — the last asserts the
  revision did not move, i.e. a refused hello writes nothing at all. Verified
  48/48 green under 6× parallel load; the same harness failed 4/30 before.
* **A turn was left `Running` whenever a runner was *stopped* rather than
  finishing its turn** — fixed in D9.20, recorded because the shape recurs. The
  runner watches for shutdown with `tokio::select!`, and a `select!` branch
  cancels its siblings, so the turn future is dropped rather than awaited; no
  code after its last await point ran, and the row stayed `Running` forever with
  nobody to close it (the runner was still alive, so boot recovery never saw
  it). `wait_for_idle` believes the store on purpose, so one leaked row made a
  whole team look permanently busy: an intermittent `client_contract` failure
  with `busy=[]` and a `llm_calls: 0` turn. The repair is a drop guard
  (`TurnGuard`) created where the rows are written, not a cleanup in the caller
  — a caller can only cover the exit it knows about. **If you add an await point
  after a row is inserted, put the settle in a guard.** Two lessons worth
  keeping: `settle()` now prints the work report *before and after* the wait,
  which is what distinguished "still running" from "leaked"; and a mutation check
  is not optional here — with the guard neutered the test failed 5 times in 10,
  and the first, wrong fix passed the unit test while leaving the real path
  broken.
* **A test that restores a source file from a temp copy will silently keep the
  mutation.** Twice in one session a mutation check "restored" a file from
  `/tmp/…bak`, and the restore went to a path that no longer existed (each `bash`
  call is a fresh sandbox with its own temp area). The mutation stayed, the unit
  test kept passing, and only a later full gate showed the file was wrong. Rules
  that would have prevented it: keep the backup **inside the workspace**
  (`.scratch/`, deleted afterwards), and after restoring, assert the fix is
  present with a `grep` before believing the result. The lesson is the same one
  the debt entries above record — **verify the state, not the intention** — and it
  applies to the agent's own edits, not just to the code under test.
* **A crypto provider must be installed before *any* rustls config is built, and
  a test that passes alone can hide that.** `rustls` picks a provider from crate
  features only when exactly one of `aws-lc-rs`/`ring` is enabled; here both are
  (two dependencies enable one each, and Cargo features add), so
  `ClientConfig::builder()` **panics** rather than returning an error. The brain's
  listener got the fix first (D9.25); the node's `--brain-ca` connector did not, and
  it passed when run on its own — a standalone test run had already installed a
  provider earlier in the same process. **The full gate caught it, not the focused
  test.** Two rules follow: the helper lives in **one** place
  (`node_protocol::ensure_crypto_provider`, called by both endpoints) so copies
  cannot drift, and a TLS test is not proven until it has passed as part of the
  whole suite, in a fresh process.
* **One "`check.sh` hung" was not reproducible, and the honest entry is "unexplained"
  until it is.** A full gate run sat in `cli`'s
  `script::tests::the_shipped_example_runs_a_real_delegation_end_to_end` for over
  30 minutes, with the process in `__futex_wait` (0 % CPU: parked, not spinning)
  and no way to take a backtrace (`ptrace_scope=1`, no permission). The same test
  alone passes in **0.33 s**, ten times in a row, and the whole `swarmbot` test
  binary passes in 2.4 s; an immediate clean full gate passed in 54 s. Two things
  were different in the failing run and both are plausible rather than proven:
  another agent was rewriting `crates/core/src/runners.rs` and
  `crates/core/tests/budget.rs` while that build was in flight, and a
  `swarmbot-ui` I had started offline was still running with its own brain on
  port 8799. **Do not call this a flake without the state in hand** — the same
  rule as the `real_brain` entries above. If it recurs, do not wait it out:
  `timeout 120 cargo test -p swarmbot-cli --bin swarmbot -- --test-threads=1`,
  and check first for a source rewrite or a stray process holding a port, because
  "the test hung" and "the thing it talks to was changing underneath it" look
  identical from a `futex_wait`.
* **A green `probe` is not a working turn, and the difference cost a day.**
  `probe` is spawn + `initialize` + `session/new`, so on 2026-11-14 all three ACP
  agents on this box reported `ok: true` while only **opencode** could answer a
  turn: `claude-code-acp` refused `session/prompt` with `Authentication required`
  (the `claude` CLI says `Not logged in`), and `codex-acp` had its every
  advertised model rejected by the owner's ChatGPT account — reproduced by
  `codex exec` outside SwarmBot, so it is the account, not the adapter. The
  config therefore names opencode only, with `model` pinned (no model ⇒
  opencode's own unroutable default ⇒ `no provider's ask matches your
  max-per-mtok bid`), and `scripts/opencode-live.sh [backend-id]` is the command
  that answers "can this box complete a turn?" because it runs the `#[ignore]`d
  live test and asserts the reply. **Before trusting any provider row, run
  that, not `/api/backends/probe`.** D9.28.
* **The embedded CLI gives an ACP bot no team tools, and now says so.** Roster
  tools (`handoff_task`, `message_bot`, `post_to_group`) reach an agent that owns
  its tool loop only through the brain's MCP gateway; `swarmbot` without
  `--brain` sets no provider, so the agent gets `mcpServers: []` and can only
  answer for itself. Observed: a `run` reporting `delegated: nothing` while the
  model claimed it had delegated. `notice::embedded_agent_without_team_tools`
  now prints two lines on stderr before the first message (D9.29); **do not
  "fix" it by faking a handoff.** Use `--brain` or `swarmbot-ui`.
* **`delegated:` counts `handoff_task` rows, not messages, and now says which
  tool was used instead.** A team that coordinated with `message_bot` made no
  task and used to print a bare `delegated: nothing` — true about tasks, and
  easy to read as "the team did nothing" (D9.30). `team_tool_hint` names the
  coordination tool with first-use order and `×n` counts, and deliberately skips
  `handoff_task` (that call already produced a row). Test:
  `a_message_bot_run_names_the_tool_instead_of_a_bare_nothing`.
* **An agent's own tool calls are transcribed as *the agent's*, and the event
  type is the guard.** ACP reports tool activity through `session/update`, and
  the adapter emits it as `LlmEvent::AgentToolCall` — **never** as
  `ToolCallStart`, because the runner executes what those events describe. If a
  future edit "simplifies" the two into one variant, SwarmBot will re-run, under
  our policy and our node, commands the agent already ran in its own sandbox.
  Rows carry `ToolCallEntry.origin` (`Swarm` | `Agent`, absent = `Swarm` for old
  transcripts) and an agent row has no `node_id`; render both, as
  `agent's own tool`, or "no node recorded" reads as a bug (D9.31). Merging one
  call's several notifications is the other trap: the terminal update repeats
  `locations` and **omits** `rawInput`, so last-wins erases the real arguments;
  and only the announcement carries the tool's **name** (`shell`), while the
  updates' `title` is the *command* (`ls crates | wc -l`) — mixing them renames
  the row mid-turn. Both were observed on live runs, not reasoned about.
* **A bot works where `settings.agent_cwd` says, and `permission_mode` is only a
  boundary when the agent asks.** `agent_cwd` (absent = the bot's private
  workspace) is handed to the backend as `ChatRequest::cwd`; the instruction text
  says "session directory … (shared)" rather than claiming privacy (D9.32). And
  measured 2026-11-14: **`opencode acp` sends no permission ask at all** with a
  stock config, so `permission_mode = "reject"` let a write through — nothing was
  ever asked. The shipped config puts `permission: {edit:"ask", bash:"ask"}` in
  opencode's `model_env`, which is what makes `reject` refuse. **Do not delete
  that key as redundant.** An agent that ends its own turn (`stopReason:
  "cancelled"`, what a denial produces) is a normal ending, not
  `LlmError::Cancelled` — that mapping made a healthy backend print
  "unavailable: cancelled" right after a denied call.
* **The embedded CLI cannot reach a remote node** — not a missing wire but a
  property of the mode: a node dials the brain's gateway, and `swarmbot` in-process
  has no listener, so nothing can pair with it. A bot configured against a foreign
  node gets a typed refusal. Use `--brain` when remote nodes matter. See
  [`node-protocol.md`](docs/node-protocol.md) §13 item 6.

## State of the tree

Check before you assume — it moves while people work. **All twelve crates are
built and tested** (last `scripts/check.sh`: fmt + clippy `-D warnings` + **1.396
passed, 0 failed, 5 ignored** — three live tests (two behind `PROTO_LIVE_LLM`, one
behind `PROTO_LIVE_ACP`) and two doctests in `store/src/threaded.rs` marked
`ignore` because a doctest has no runtime to await on): `contracts`,
`store`, `llm`, `tools`, `core`, `client`, `node-protocol`, `brain`, `cli`, `node`,
`ui` and `mcp`.

The brain's **node gateway is real**: `POST /node/pair` redeems a code and
`GET /node` serves the signed control socket (`src/gateway/`).
**`crates/node/tests/real_brain.rs` pins the whole of M6 against the real
binaries** (5 tests, ~0.5 s, always run; the fifth is the `fs.download` data
stream of D9.21): an in-process brain + a spawned
`swarmbot-node` child pair with a minted code, a bot bound to that node reads
`marker.txt` and runs `cat marker.txt` through it — asserted by planting
"from the node" in the node's root and a "from the brain" decoy in the brain's
workspace, so a silent local fallback cannot pass; the node's own
`--ceiling shell=never` refuses what the brain allowed (`this node never allows
…`, nothing runs on either machine); and `DELETE /api/nodes/{id}` closes with
**4401**, the child exits by itself, and a redial is refused before any upgrade.
`crates/node/tests/tls_brain.rs` adds 2 more: the real node pairs over `https://`,
completes the signed upgrade and `node.hello` over `wss://` (D9.25), and a node
without `--brain-ca` is refused at the handshake.
**`crates/node/tests/live_two_process.rs` closes the last one** (D9.26): a real
brain **process**, a real node **process**, and a real model (`PROTO_LIVE_LLM`),
with a decoy marker on the brain side so a silent local fallback fails. It is
`#[ignore]`d because it needs a gateway credential.

`llm::ClaudeCli::chat` is implemented: it spawns `claude -p` with a stream-json
protocol and reaches our tools through `--mcp-config`. The MCP gateway exists
(`crates/mcp`), and `/mcp` on the internal listener is authenticated with the
owner token like every other execution surface.
The honest test of "is this module wired?" is still `crates/<name>/src/lib.rs`.
[`README.md`](README.md#status-milestone) carries the milestone table and the list
of paths nobody has verified yet.

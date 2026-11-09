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
and pins `VK_ICD_FILENAMES` to llvmpipe. Without it a `cargo build -p swarmbot-ui`
fails in three different ways.

Cargo builds share `./target`. `Blocking waiting for file lock on build directory`
is other people working, not a hang: wait it out. Do not invent a second target
directory — it recompiles every shared dependency.

This checkout is **not a git repository yet**. Do not `git init`, do not commit,
do not tag, unless the owner asks.

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
  `PROTO_LIVE_MODEL`), `PROTO_LIVE_CLAUDE=1` and `PROTO_LIVE_ACP=1` (plus
  `PROTO_LIVE_ACP_CMD`/`_ARGS`; free handshake unless `PROTO_LIVE_ACP_PROMPT=1`).
  They stay trivially cheap — one turn — and they print what they spent.
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
| `store` | `SqliteStore`, `MemoryStore`, `JsonlMirror`, the shared contract suite | `src/lib.rs`, `src/contract.rs` |
| `core` | runners, orchestrator, routing, messaging, tasks, approvals, transcript, live hub, instructions, composition | `src/lib.rs`, `src/runners.rs`, `src/swarm.rs` |
| `client` | the blocking REST + live-WS client shared by CLI and UI | `src/lib.rs` |
| `brain` | composition root: axum REST, `/ws/live`, owner auth, telemetry, and the node gateway (`/node/pair`, the signed `/node` upgrade, `NodeDispatcher`) | `src/lib.rs`, `src/server.rs` (the route table), `src/routes/`, `src/gateway/` |
| `cli` | `swarmbot`: demo, run, chat, bots, transcript, tasks, nodes, config, shutdown — embedded *and* `--brain` | `src/main.rs` (commands), `src/session.rs` (the `Session` port both modes implement), `src/remote.rs` |
| `ui` | `swarmbot-ui`: the GPUI desktop app | `src/lib.rs` (module map), `src/reducer.rs` (where the correctness lives), [`docs/ui.md`](docs/ui.md) |
| `node-protocol` | the wire: mux, pairing, signed upgrade, typed errors, stream tokens | `src/lib.rs` (module map), [`docs/node-protocol.md`](docs/node-protocol.md) |
| `node` | `swarmbot-node`: the daemon that executes under its own policy | `src/lib.rs`, `src/handlers.rs` (the `rpc` ↔ `NodeCall` bridge), `src/shell.rs`, `tests/real_brain.rs` (the end-to-end proof) |
| `mcp` | the tool registry as an MCP server (stdio + streamable HTTP) | *(M3)* |

## Known debt

Two things recorded on purpose, so nobody "fixes" them by accident or trusts them
further than they go:

* **`crates/node/src/shell.rs` mirrors `tools::handlers::shell::exec_shell`**
  instead of calling it, because that function is `pub(crate)` and `LocalExecutor`'s
  *buffered* result cannot produce `Started{pid,pgid}`, Ack-paced chunks and a
  verifiable `killpg`. Its module header lists the **six things that must stay in
  sync**. The way out is to make `exec_shell` public — or add a streaming variant in
  `tools` — and delete the copy; until then, a change to either side is a change to
  both.
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
* **One `crates/node/tests/real_brain.rs` test is suspected flaky under
  machine load.** `the_nodes_own_never_ceiling_refuses_what_the_brain_allowed`
  failed once in four full `check.sh` runs (the transcript had no `read_file`
  call) and passed 3/3 in isolation. It spawns a real `swarmbot-node` child and a
  real brain, so it is the most load-sensitive test in the tree; on a 4-core box
  shared with other builds a bounded wait can be starved. Same rule as the CLI
  flake above: re-run before you debug, and if it reproduces, widen the specific
  wait and name what it waits for — do not add a blanket sleep and do not
  `#[ignore]` it.
* **The node's spill dir `<data>/spill` is not inside a root by default** (the only
  default root is `<data>/workspace`), so a bot cannot `fs.read` a spilled file back
  unless the owner adds a root covering it. See
  [`node-protocol.md`](docs/node-protocol.md) §13.

## State of the tree

Check before you assume — it moves while people work. As of this writing **eleven
of the twelve crates are built and tested** (last `scripts/check.sh`: fmt + clippy `-D warnings` + **1021
passed, 0 failed, 2 ignored** — the ignored ones are the live-gateway LLM test behind
`PROTO_LIVE_LLM` and the live ACP-agent test behind `PROTO_LIVE_ACP`): `contracts`, `store`, `llm`, `tools`, `core`, `client`,
`node-protocol`, `brain`, `cli`, `node` and `ui`. `crates/mcp` is the only
one-line scaffold (milestone M3).

The brain's **node gateway is real**: `POST /node/pair` redeems a code and
`GET /node` serves the signed control socket (`src/gateway/`).
**`crates/node/tests/real_brain.rs` pins the whole of M6 against the real
binaries** (3 tests, ~0.4 s, always run): an in-process brain + a spawned
`swarmbot-node` child pair with a minted code, a bot bound to that node reads
`marker.txt` and runs `cat marker.txt` through it — asserted by planting
"from the node" in the node's root and a "from the brain" decoy in the brain's
workspace, so a silent local fallback cannot pass; the node's own
`--ceiling shell=never` refuses what the brain allowed (`this node never allows
…`, nothing runs on either machine); and `DELETE /api/nodes/{id}` closes with
**4401**, the child exits by itself, and a redial is refused before any upgrade.
Still genuinely unverified: `wss://`/TLS, and a remote tool call from a *bot turn
in two separate OS processes driven by a real model* (the fake backend's script
lives in-process, so the test drives the brain in-process).

`llm::ClaudeCli::chat` still returns `Unavailable` until the MCP gateway exists —
deliberately, so a bot on that backend fails with a reason instead of pretending.
The honest test of "is this module wired?" is still `crates/<name>/src/lib.rs`.
[`README.md`](README.md#status-milestone) carries the milestone table and the list
of paths nobody has verified yet.

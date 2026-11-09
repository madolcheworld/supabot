# SwarmBot tools

Every tool a bot can call, what it takes, what it returns, and which safety rule
governs it. The authoritative definition of each tool is its `spec()` function —
`crates/tools/src/handlers/<tool>.rs` for the seven node tools, the
`BotToolKind::spec` match in `crates/core/src/bot_tools.rs` for the thirteen bot
tools. `ToolRegistry::dispatch` validates arguments against that spec's JSON
Schema (`jsonschema` crate) before any handler runs, so this page, the registry
and the model all see one contract.

**What exists in this build:** both halves of the catalogue, implemented and
tested —

* the seven **node tools** (`crates/tools/src/handlers/`, 94 tests): files, shell,
  search;
* the thirteen **bot tools** (`crates/core/src/bot_tools.rs`, registered by
  `register_bot_tools`, which `Swarm::new` calls while it builds the registry):
  talking, delegating, asking the human, seeing the roster.

Nothing else is offered to a model. In particular there is no browser, desktop,
file-transfer or routine tool: those are out of scope
([`decisions.md`](decisions.md) D0.4) and are not silently absent — a node asked
for one answers with a typed refusal ([`node-protocol.md`](node-protocol.md) §9).

---

## 1. Result rules (every tool, no exceptions)

Enforced once, in `ToolRegistry::dispatch` / `apply_result_rules`, not per handler:

| Rule | Value / behaviour |
| --- | --- |
| Content shape | `ToolResult { content: Vec<Content>, is_error, meta }` — text (and later images). No free-form `structuredContent`. |
| Errors | text starts with `ERROR: ` and reads as an error on its own (`ToolResult::error` adds the prefix exactly once). |
| Size cap | `TOOL_RESULT_LIMIT_BYTES = 10 * 1024` |
| Spill | over the cap, the full text is written to `<spool_dir>/<tool>-<invocation id>.txt` (`<data>/workspaces/<bot>/.spool/`) and the result keeps the head plus `[output too large: N bytes total. The full output was spilled to <path>. Page it with read_file {"path": …, "offset": 1, "limit": 200} or search it with grep.]` |
| Spill failure | the result is still capped — blowing up a context is worse than losing the tail |
| Argument validation | schema-invalid arguments never reach a handler: `ERROR: invalid arguments: …` (up to 3 messages, each with its instance path) |
| Unknown tool | `ERROR: unknown tool: <name>` |
| Cancellation | a tripped `ctx.cancel` (the owner pressed Stop) returns `ERROR: cancelled` — a stopped turn must not act, not even "just" read |
| Denial | ``ERROR: denied by the owner: <tool> `<target>` `` — an `Ok` result with `is_error`, because a denial is an answer the model should read, not an infrastructure failure |
| `meta` | decision/surface/target/node plus per-tool facts (sha256, exit code, duration, `truncated`). The UI and the audit line read it; the model does not need it. |

## 2. Gating: which tools a bot sees

`ToolRegistry::visible_specs(toolsets, node_caps, in_task)` applies three gates in order:

1. **Toolset** — the bot's `toolsets` must contain the tool's (`Node` for all seven below).
2. **Node capabilities** — a `Node` tool is hidden when the target node's effective
   capabilities (`declared ∩ approved`) lack `shell@1` (for `shell`) or `fs@1`
   (for the filesystem tools). `node_caps = None` means "unknown" and does **not**
   gate: a bot with no bound node keeps its tools and gets a clear error at call
   time rather than silently losing half its hands.
3. **Task context** — inside a task, `TASK_TOOLS` (`start_task`, `list_tasks`,
   `check_task`, `cancel_task`, `update_task`) and `ASK_TOOLS` (`ask_user`,
   `request_approval`) are hidden: a task that could start tasks is one confused
   model away from a recursion bomb, and there is no owner watching a task
   conversation to answer an ask.

Toolsets: the seven node tools are `Toolset::Node`; `message_bot`,
`post_to_group`, `handoff_task`, `react`, `list_bots`, `list_nodes`, `ask_user`
and `request_approval` are `Toolset::Bot`; the five `*_task` tools are
`Toolset::Task`. `Toolset::Owner` is reserved and no tool uses it yet.

Instructions never list a tool the bot does not have: `InstructionRenderer`
renders its "What you can do" section from `visible_specs` output.

## 3. Safety surfaces and ceilings

Each tool acts on one `Surface`. The effective ceiling is
`min(brain ceiling, node ceiling)` per surface — a remote brain can tighten a
node, never loosen it (`PolicyEngine::decide`).

| Surface | Tools | Brain default (`config/swarmbot.example.toml`) | Read-only auto-allow |
| --- | --- | --- | --- |
| `shell` | `shell` | `ask` | no |
| `read_file` | `read_file`, `list_dir`, `glob`, `grep` | `always` | yes (`settings.auto_approve_read_only`) |
| `write_file` | `write_file` | `ask` | no |
| `edit_file` | `edit_file` | `ask` | no |

Ceilings: `never` → `ToolError::PolicyRefused { retry: false }` with a reason that
names the surface; `ask` → the `ApprovalGate` (rules first, then a human card);
`always` → run. A destructive `shell` command is raised to `ask` even under
`always` — see [`decisions.md`](decisions.md) D5.5 for the pattern list and its
stated limits (it is not a shell parser).

Path boundary, before any of that: `PathPolicy::resolve` expands `~/`, resolves
relative paths against the bot's workspace, canonicalizes component by component
so a symlink cannot escape a root, then requires the result to be inside a root,
outside the deny list, and (for writes) inside a read-write root. Failures are
`ToolError::PathDenied { path, reason }` with reasons `outside roots`,
`deny-list`, `read-only root`.

## 4. Tool reference

### 4.1 Node tools (toolset `Node`, all accept an optional `node`)

`node` is a node id or the literal `"local"`; absent means the bot's host node
(`bot.node`, else `nod_local`). When it names a remote node the handler asks the
gate and forwards a `NodeCall` through `NodeExecutor` **without touching the local
disk** — paths are resolved by the executing machine, never by the sender.

#### `shell` — run a command

`Surface::Shell`, not read-only. Example schema (the other six follow the same
shape; all are `"additionalProperties": false`):

```json
{
  "type": "object",
  "properties": {
    "command":    { "type": "string", "minLength": 1, "description": "The command line, run with `sh -c`." },
    "cwd":        { "type": "string", "description": "Working directory (inside a node root); defaults to the bot's workspace." },
    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 900000, "description": "Kill the command after this many milliseconds; default 120000, hard maximum 900000." },
    "env":        { "type": "object", "additionalProperties": { "type": "string" }, "description": "Extra environment variables for this command only." },
    "node":       { "type": "string", "description": "Node id (or \"local\") to run on; defaults to the bot's host node." }
  },
  "required": ["command"]
}
```

| | |
| --- | --- |
| Returns | `exit=<code>` (or `signal:<n>`) then `--- stdout ---` / `--- stderr ---` sections, empty ones omitted, plus an honest note when a stream was capped. `is_error = true` on a non-zero exit — but the text keeps the `exit=<code>` shape rather than an `ERROR: ` prefix, so the code stays machine-findable. |
| `meta` | `exit_code`, `duration_ms`, `decision`, `surface`, `target` (the command line), `node` |
| Limits | `DEFAULT_SHELL_TIMEOUT_MS = 120_000`, `MAX_SHELL_TIMEOUT_MS = 900_000`; each stream capped at `SHELL_OUTPUT_LIMIT_BYTES = 1 MiB`, then the registry's spill |
| Process | own process group (`process_group(0)`); `killpg(SIGKILL)` on timeout, on cancellation, and after a normal exit whose pipes are still held by background children. Invariant: when `shell` returns, nothing it started is still attached to it |
| Environment | scrubbed to `PATH`, `HOME`, `LANG`, `LC_ALL`, `TERM`, `TMPDIR` plus the caller's `env`; a fallback `PATH` is set when the brain has none |
| Refuses | `cwd` outside a root; a `never` ceiling; destructive patterns raise it to `ask` |

#### `read_file` — read a text file

`Surface::ReadFile`, **read-only**.

| Argument | Type | Notes |
| --- | --- | --- |
| `path` * | string (minLength 1) | relative paths resolve against the workspace, `~/` expands |
| `offset` | integer ≥ 1 | 1-based first line; default 1 |
| `limit` | integer ≥ 1 | maximum lines; default `DEFAULT_READ_LINE_LIMIT = 2000` |
| `encoding` | `utf-8` \| `utf8` \| `text` | only UTF-8 is supported; the option exists so a model can say what it expects |
| `node` | string | |

Returns lines as `{line:>4}→content` — the gutter format is a contract, because
`edit_file` diffs and `grep` results quote the same numbers. An empty file returns
`(empty file)`; an offset past the end returns `(offset N is past the end; <path>
has M lines)`; otherwise the tail carries `[showing lines a–b of N; read_file with
offset=b+1 for the next page]`. `meta`: `total_lines`, `first_line`, `last_line`.
Binary or non-UTF-8 input is refused with an error that says it is not text.

#### `write_file` — atomic, optionally conditional write

`Surface::WriteFile`, not read-only.

| Argument | Type | Notes |
| --- | --- | --- |
| `path` * | string (minLength 1) | |
| `content` * | string | the full new content |
| `expected_sha256` | string | hex sha256 the file must currently have; `""` means "must not exist". Checked before anything is written |
| `create_parents` | boolean | default false |
| `node` | string | |

Returns `wrote <size> bytes to <path>` + `sha256: <hash>`; `meta`: `sha256`,
`size`, `path`. Bytes go to a temp file in the *same directory* (same filesystem,
so the rename is atomic), are fsynced, then renamed over the target: a reader
never sees a half-written file. Mismatches are errors that name both hashes.

#### `edit_file` — exact-string replacement

`Surface::EditFile`, not read-only.

| Argument | Type | Notes |
| --- | --- | --- |
| `path` * | string (minLength 1) | |
| `old_string` * | string (minLength 1) | must occur exactly once unless `replace_all` |
| `new_string` * | string | |
| `replace_all` | boolean | default false |
| `node` | string | |

Returns a unified diff (`a/<path>` / `b/<path>` headers) followed by
`sha256: <hash>`, or `no change (old_string equals new_string)` + the hash.
`meta`: `sha256`, `replacements`, `path`. An ambiguous match is refused with the
match count and the advice to extend `old_string` or pass `replace_all` — never
"first wins".

#### `list_dir` — what is on disk

`Surface::ReadFile`, **read-only**.

| Argument | Type | Notes |
| --- | --- | --- |
| `path` * | string (minLength 1) | |
| `depth` | integer ≥ 0 | 1 (default) = immediate children, 0 = unlimited |
| `include_hidden` | boolean | default false |
| `limit` | integer ≥ 1 | default `DEFAULT_LIST_LIMIT = 1000` |
| `node` | string | |

Returns sorted `type size path` lines (`d`/`f`/`l`; size in bytes, `-` for
directories). Deliberately dumb: it honours **no** ignore file, which is the
honest division of labour with `glob`/`grep` (which do), and its description says
so.

#### `glob` — find paths by pattern

`Surface::ReadFile`, **read-only**.

| Argument | Type | Notes |
| --- | --- | --- |
| `pattern` * | string (minLength 1) | `*`, `?`, `[...]`, `**`. A pattern without `/` matches the file name at any depth (`*.rs` finds `src/main.rs`); with `/` it matches paths relative to `path` |
| `path` | string | directory to walk; default the workspace |
| `limit` | integer ≥ 1 | default `DEFAULT_GLOB_LIMIT = 200` |
| `node` | string | |

Returns matching paths relative to `path`, sorted; over the limit it appends
`[truncated: showing the first N of M matches; narrow the pattern]`. `meta`:
`truncated`. The walker is the `ignore` crate's (the one ripgrep uses), so
`.gitignore`/`.ignore` are respected and hidden files stay out unless asked for.

#### `grep` — content search

`Surface::ReadFile`, **read-only**.

| Argument | Type | Notes |
| --- | --- | --- |
| `pattern` * | string (minLength 1) | Rust regex syntax, or literal text with `literal: true` |
| `path` | string | file or directory; default the workspace |
| `glob` | string | only search files matching this glob (same semantics as the `glob` tool) |
| `ignore_case` | boolean | default false |
| `literal` | boolean | default false |
| `context_lines` | integer ≥ 0 | default 0 |
| `output_mode` | `content` \| `files_with_matches` \| `count` | default `content` |
| `limit` | integer ≥ 1 | default `DEFAULT_GREP_LIMIT = 200` |
| `node` | string | |

`content` prints `path:line:text`, context lines as `path-line-text`, and `--`
between non-adjacent groups, so a model can lift line numbers straight into
`read_file` or `edit_file`. `files_with_matches` prints one path per file; `count`
prints `path:matches`. Ignore-aware, hidden files and symlinks skipped, binaries
skipped.

### 4.2 Bot tools (toolset `Bot` / `Task`, registered by `core::bot_tools`)

Thirteen tools, one shared environment (`BotToolEnv`: store, hub, transcript,
`Messaging`, `BackgroundTasks`, `ApprovalService`, `RunnerMap`). Three rules shape
all of them (module docs of `bot_tools.rs`):

* **never block on a human** — `ask_user` and `request_approval` raise a card and
  return at once;
* **the services decide, the tools translate** — the hop limit, "group work stays
  in its group", the task lifecycle and the approval rules are enforced by
  `Messaging` / `BackgroundTasks` / `ApprovalService`; a handler only resolves the
  caller, calls the service, and turns the outcome into a line a model can act on;
* **results carry the ids the next call needs** — a `tsk_…` or `req_…` id is part
  of the result text, because `check_task` and the answer delivery refer to it.

A stopped chain is a **result, not an error** ("Stopped: … the chain ends with you
— answer in this conversation instead"), and a service error becomes text that
says what to do instead ("there is no bot called `nobody`; the team is @lead,
@scout"). None of these tools has a safety surface: they change no filesystem and
run no command, so they never raise an approval card of their own.

| Tool | Toolset | Read-only | Arguments (`*` = required) | Returns |
| --- | --- | --- | --- | --- |
| `message_bot` | `Bot` | no | `to*` (handle, `@` optional), `text*`, `urgency` (`urgent` \| `background`, default `background`) | `Messaged @x in their DM.` or `… in this group — a message to a fellow member stays where the owner can see it.`, plus how it landed: woken / woken-and-flagged-urgent / `They are mid-turn right now; your message waits for that turn to end.` / `They were NOT woken (a disabled bot stays asleep)…`. Messaging yourself is refused (`that is you; there is nobody to message`) |
| `post_to_group` | `Bot` | no | `group*` (title or `cnv_…` id), `text*` | `Posted in "<title>". Teammates you @mentioned were woken and will read it on their next turn.` |
| `handoff_task` | `Bot` | no | `to*`, `title*`, `task*` (self-contained), `context?` | `Handed "<title>" to @x as task tsk_…. It is theirs to run; when they finish, their result comes back to this conversation. Follow progress with check_task.` A handoff to yourself is refused with `use start_task for your own work` |
| `react` | `Bot` | no | `emoji*` | `Your 👍 reaction was recorded in the transcript. Nobody was woken — if you have something to say, use message_bot.` |
| `list_bots` | `Bot` | **yes** | — | `- @name (Label) (you) — <first line of its persona> [idle\|working\|queued\|disabled\|error]` |
| `list_nodes` | `Bot` | **yes** | — | `- <label> (<nod_…>) [local\|remote node, online \| OFFLINE — it cannot run anything until it reconnects \| REVOKED — it may not reconnect] — capabilities: fs@1, shell@1 \| none approved; roots: work: /path (read-write\|read-only)`. With no nodes: `No nodes are paired. Your tools run on the brain's built-in local node.` |
| `ask_user` | `Bot` | no | `question*`, `options?` (array of strings → a choice field; absent → one free-text field) | `{"status":"pending","id":"req_…"} — the owner is being asked your question. This call did not wait: the answer arrives later as a message that wakes you. Carry on with other work, or end your turn; do not ask twice.` |
| `request_approval` | `Bot` | no | `action*`, `reason?` | ``{"status":"pending","id":"req_…"} — the owner is being asked to approve `<action>`. This call did not wait and did NOT run anything: … Only proceed with the action once that message says it was allowed.`` |
| `start_task` | `Task` | no | `title*`, `brief*` (the task sees only this) | `Started "<title>" as your background task tsk_… (queued\|running). It runs while you stay free here; when it finishes, its result comes back to this conversation.` |
| `list_tasks` | `Task` | **yes** | — | your tasks with status and title; the oldest are elided past `MAX_LISTED_TASKS = 30` |
| `check_task` | `Task` | **yes** | `task_id*` | head line (`"<title>" (tsk_…) — <status>`, plus owner/requester relative to you), then `Result: …` / `No result yet.` / `It ended (<status>) without a written result.`, then `Context from the sender: …`, then the last `MAX_TASK_STEPS = 20` steps as `N. [kind] text`. **Any** task on the team may be read — a requester follows its handoff, and the delegation graph is not a secret |
| `update_task` | `Task` | no | `task_id*`, `text*` | `Added to "<title>" (tsk_…): it was steered straight into the turn working on the task.` — or, when it could not be steered, that the note is on the task record and will be read when it next runs. Only the task's **owner** or its **requester** may update; anybody else is told to message the owner |
| `cancel_task` | `Task` | no | `task_id*` | `Cancelled "<title>" (tsk_…). No result will be delivered.` — or `was already <status> — nothing to cancel.` Only the **owner** may cancel (cancelling interrupts that bot's turn); a requester messages the owner instead. Cancelled by the bot itself, so no event line and no result delivery |

Both hop-limit and kill-switch behaviour come from `Messaging`: at
`settings.max_hops` (default 8) the chain stops with the text above **and** an
event line in the transcript; with `settings.bot_messaging = false` the call fails
with `bot messaging is switched off by the owner`.

One asymmetry worth knowing: `message_bot` and `post_to_group` pass the chain on
(one hop further), but `handoff_task` hands the task to `BackgroundTasks::submit`,
and the delivery the pump builds starts a **fresh** chain. So `max_hops` bounds
message chains, while a handoff-to-handoff chain is bounded by
`max_running_tasks_per_bot` and `max_tool_rounds` instead
([`architecture.md`](architecture.md) §4).

## 5. Node calls (the wire shape, and who runs them)

`tools::NodeCall` is the RPC payload as data — `#[serde(tag = "call",
rename_all = "snake_case")]`, so `{"call":"shell_exec","command":"…"}` is what a
remote node receives. It carries `surface()`, `read_only()` and `describe()`
(a target string for cards and audit lines that never includes file contents),
which is how a node applies its own ceilings as a backstop.

| Tool | `NodeCall` | `surface()` | `read_only()` |
| --- | --- | --- | --- |
| `shell` | `ShellExec {command, cwd?, timeout_ms?, env?}` | `shell` | no |
| `read_file` | `FsRead {path, offset?, limit?, encoding?}` | `read_file` | yes |
| `write_file` | `FsWrite {path, content, expected_sha256?, create_parents?}` | `write_file` | no |
| `edit_file` | `FsEdit {path, old_string, new_string, replace_all?}` | `edit_file` | no |
| `list_dir` | `FsList {path, depth?, include_hidden?, limit?}` | `read_file` | yes |
| `glob` | `FsGlob {pattern, path?, limit?}` | `read_file` | yes |
| `grep` | `FsGrep {pattern, path?, glob?, ignore_case?, literal?, context_lines?, output_mode?, limit?}` | `read_file` | yes |
| — | `FsStat {path}`, `FsMkdir {path, parents?}`, `FsMove {from, to}`, `FsRemove {path, recursive?}` | `read_file` / `write_file` | mixed |

`NodeCall::surface()` returns the same surface the local handler asks on —
`FsEdit` is `edit_file`, not `write_file` — so tightening one surface governs a
remote call exactly as it governs a local one. `LocalExecutor` agrees by
construction: its `run` arms call `check_ceiling` with the matching surface
(`FsEdit` → `Surface::EditFile`) as the node's own backstop.

`FsStat`, `FsMkdir`, `FsMove` and `FsRemove` have no model-facing tool today: they
exist so a node daemon can serve the operations the local handlers need
internally (and so `write_file`'s atomic rename has a remote equivalent).

`NodeCall` is the *local* shape, not the wire shape: the `node-protocol` crate has
its own param structs (`rpc::FsRead`, `rpc::ShellExec`, …) which additionally carry
`inv`, and the two are not field-for-field identical. `crates/node`'s
`handlers::to_node_call` is the bridge, and its rule is that **a condition the
bridge cannot carry is refused, never dropped** (D9.8): `fs.write`'s
`expectedSha256` maps straight through and is enforced, while `fs.edit`'s has no
`NodeCall` counterpart and is answered with `PolicyRefused { retry: false }`. The
same goes for a `base64` body (the local implementation is text-only), and
`FsMove.overwrite: false` is enforced by an existence pre-check that answers
`FsError { code: "EEXIST" }`. [`node-protocol.md`](node-protocol.md) §5 and §5.1
carry the full mapping table.

One consequence for the result rules in §1: a remote `fs.*` call returns the
serialized `contracts::ToolResult` this crate produced (D9.7), so the cap, the
`ERROR: ` prefix and the spill note survive the trip unchanged — the brain's remote
`NodeExecutor` deserializes straight back into the type its own port returns.

Two implementations of `NodeExecutor` exist and both are tested:

* **`LocalExecutor`** runs a call in process and is what makes the brain's machine
  `nod_local`. It knows **which node it is** (`node_id`, default `nod_local`, set
  with `for_node`) and **refuses a call addressed to any other node** with
  `PolicyRefused { retry: false }` rather than absorbing it (D9.11). Absorbing was
  the bug: a bot bound to a remote node had its tools executed on the brain's
  machine, inside the brain's roots and under the brain's ceilings, and nothing in
  the transcript showed it — the tool-call entry still named the node the model asked
  for. A node daemon passes its own paired id, so calls addressed to itself are its
  own to run. Known consequence: the embedded CLI composes through `core::compose`,
  which has no dispatcher in front of the executor, so a remote-bound bot there gets
  a clear typed refusal instead of remote execution.
* **`brain::gateway::executor::NodeDispatcher`** routes a call to the local machine
  or across a connected node's control socket, and fails rather than falling back
  locally when the node is not connected. Because `NodeExecutor::exec` does not
  carry the `Invocation` (D9.12, an open proposal), the dispatcher *derives*
  attribution for the wire's `inv`: exactly one running turn belonging to a bot
  hosted on that node ⇒ that turn, otherwise a `bot_unknown`/`cnv_unknown` sentinel
  with a distinct invocation id — never a guess.

Policy split, stated precisely: ceilings and approvals are decided by the **caller**
before a `NodeCall` arrives; the executor enforces what only the executing machine
can enforce — the `PathPolicy` boundary, and, when given an engine, its `Never`
ceilings as a backstop, so even a buggy or malicious brain cannot make a node do what
its own policy says it never does. `Ask` at that layer means "was asked upstream":
with no human attached to an executor, re-asking would deadlock the call.

## 6. Audit line

`AuditLog` writes JSONL to `<data>/audit/<node id>.jsonl`, one line per dispatch:

`ts`, `invocation_id`, `bot_id`, `turn_id?`, `task_id?`, `node_id?`, `tool`,
`surface`, `target` (command line or path — never file contents), `decision`
(`run` / `ask` / `allow` / `deny` / `refuse`), `outcome` (`ok` / `error`),
`duration_ms`, `result_bytes`.

Invariant: the audit log never fails a tool call. An append that errors is logged
loudly and dropped, because an audit log that breaks the work is worse than one
that misses a line.

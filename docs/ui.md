# SwarmBot desktop UI (GPUI)

`crates/ui`, binary `swarmbot-ui`. A native desktop app that watches bots
cooperate: the roster, the transcripts, the delegation graph, and the cards that
need a human.

**Current state: the app is built.** 16,613 lines across 29 source files, **85
tests**, one window with a sidebar, a transcript, a composer, a delegation-graph
panel, cards, a nodes screen, a routines screen, a usage screen, a settings screen
and a command palette. It is a pure client of the brain's REST + `/ws/live` API, so
it needs a brain to talk to — and `--offline --spawn-brain` starts one with the
scripted `fake` backend, which is a genuine zero-credential demo (§5, §7).

Several modules cite this document by section number (`view/cards.rs` → §3,
`widgets/*` → §4, `brain.rs` → §6, `reducer.rs` → §3). Those numbers are
load-bearing: keep them where they are when editing.

---

## 1. Process and executor boundary

Two runtimes, two processes, one loopback socket between them (D7.2):

| | owns | never |
| --- | --- | --- |
| `swarmbot-brain` | tokio: runners, model calls, SQLite, tools, HTTP | GPUI, the main thread |
| `swarmbot-ui` | the main thread and GPUI's own executor: window, event loop, rendering | tokio, `core`, `store`, SQLite |

Inside the UI process there are **two** blocking worker threads, not one, and the
reason is in `worker.rs`: `LiveSubscription::next_event` blocks until a frame
arrives and exposes no read timeout, so a single thread parked on the socket could
not also service a Stop button or a card answer promptly.

```
main thread (GPUI)                      live thread            rpc thread
  Entity<AppState>                        /ws/live pump          one blocking REST
  Render for each view                    reconnection           call per Command
  foreground timer drains an mpsc  ◄────── WorkerMsg ──────┬──────┘
  and applies it through reducer::apply                     │
  inside Entity::update                              both share one BrainClient
```

Rules that follow from the split:

* **no tokio anywhere in the UI process** — one executor per thread, and the
  invariant D7.2 actually cares about (no I/O on the main thread) still holds;
* the workers hold **no GPUI handle**: they only push `proto::WorkerMsg` onto an
  mpsc channel, and the main thread applies it through the reducer inside
  `Entity::update`, so every state change lands as a GPUI notify on the UI thread.
  The pump runs on a foreground timer (`POLL_INTERVAL = 40 ms`), with a
  `TICK_INTERVAL = 200 ms` for time-driven redraws (expiry countdowns, spinners);
* both threads share one `BrainClient`; its clones share the token and cookie jar,
  so a `login()` on the rpc thread is seen by the live thread's next reconnect;
* the live thread owns reconnection: `Backoff` from 250 ms to 8 s, and it resumes
  from the newest `LiveCursor` it saw, so a blip costs no transcript;
* the UI can point at a brain on another machine (`--brain`), because nothing in it
  assumes a shared filesystem or database. `--spawn-brain` supervises
  `swarmbot-brain` as a child for a one-command start — a supervisor, not an
  embedding: the child is a separate process spoken to over loopback HTTP exactly
  like a remote brain, and everything in `spawn.rs` is `std::process`.

## 2. Verified environment facts (this machine)

| Fact | Consequence |
| --- | --- |
| `gpui` is published on crates.io; the workspace pins `=0.2.2` exactly | pre-1.0 with frequent breaking changes, so an upgrade is a decision, not a `cargo update` (D8.4) |
| built `default-features = false, features = ["font-kit", "wayland"]` | gpui's Linux default also enables `x11`; this box has the `libxkbcommon-x11` **runtime** but not the dev symlink, so the `x11` feature would not link. The session is Wayland (`WAYLAND_DISPLAY=wayland-1` on 2026-11-14, running Hyprland; an earlier note said KDE Plasma / `wayland-0` — the compositor is whatever the owner is logged into, so do not treat either name as fixed) |
| `pkg-config xkbcommon-x11` missing, `libxkbcommon-x11.so.0` present | `scripts/env.sh` creates `.cargo-home/link-shim/libxkbcommon-x11.so` → the runtime `.so.0` and exports `LIBRARY_PATH`. `LIBRARY_PATH` (unlike `RUSTFLAGS`) does not invalidate the cargo cache, so only the final link is redone. The clean fix is `sudo dnf install libxkbcommon-x11-devel` |
| `gpui`'s build script runs `bindgen`; `libclang.so.22.1` exists without an unversioned `libclang.so` | `scripts/env.sh` exports `LIBCLANG_PATH=/usr/lib64` |
| `gpui → gpui_http_client → zed-async-tar → xattr 0.2`, and `xattr 0.2.3` no longer compiles against modern `libc` (`ENOATTR` is `ENODATA` on Linux); it is an optional dep enabled by a default feature a downstream workspace cannot turn off | `vendor/xattr` is the crate with a one-line patch, wired in by `[patch.crates-io]` in the root `Cargo.toml`. See [`../vendor/xattr/VENDORED.md`](../vendor/xattr/VENDORED.md) |
| **The GPU works (re-checked 2026-11-14).** `/dev/nvidia0`, `/dev/nvidiactl`, `/dev/nvidia-modeset`, `/dev/nvidia-uvm`, `/dev/dri/card1` and `/dev/dri/renderD128` all exist; `nvidia-smi` reports a GTX 1660 SUPER on driver 615.78.08; `nvidia_icd.json` is the ICD in use | `./target/debug/swarmbot-ui --selftest` **opens 11 windows and exits 0**, with `vulkan/renderer: icd=/usr/share/vulkan/icd.d/nvidia_icd.json`. The *render total* it prints is not stable — 22, 54, 55, 57 and 58 across five runs of one binary — so assert on the 11 windows and on `ok=true`, never on a render count. The 2026-11-13 entry here said the opposite (`NoSupportedDeviceFound` panic, no usable ICD) and that was true then; it is not true now. `scripts/env.sh` still prefers llvmpipe *when that ICD exists* and still warns on stderr when none does — a headless-box fallback, not a statement about this machine |
| `pkg-config vulkan` missing, `libvulkan.so.1` present | enough: blade loads the Vulkan library at runtime |
| the crate has a `runtime_shaders` feature (off by default) | enables it for shader iteration without a build-time shader toolchain; leave it off for normal builds |
| `swarmbot-ui` is **not** in the workspace `default-members` | plain `cargo build`/`cargo test` skip it (its dependency chain is minutes long); `--workspace`, `scripts/check.sh` include it, `scripts/check.sh --fast` excludes it again |

## 3. Screens

One window: a left sidebar (`SIDEBAR_W = 248 px`), a main pane, and a toggleable
delegation-graph panel on the right (`GRAPH_W = 320 px`). Dark first; no xAI/Grok
fonts, logos, icons or avatar art (D0.3) — an avatar is a tile in the bot's colour
with its initial, and the colour comes from `Bot::effective_color` (FNV-1a over the
id → hue → `#rrggbb`) unless the roster names one.

| Screen | Module | Shows | Live events that drive it |
| --- | --- | --- | --- |
| Root layout | `view/mod.rs` | sidebar + main pane + graph panel, the header, overlays | — |
| Sidebar | `view/sidebar.rs` | bots (avatar tile, label, status line, activity), conversations (DM and group, preview, unread), nodes (online/offline, capabilities), settings | `BotUpsert`, `BotStatus`, `ConversationUpsert`, `NodeUpsert`, `NodePresence` |
| Conversation | `view/conversation.rs` | transcript: **owner bubbles right, bot bubbles left**, event lines (`Messaged @scout`, `Handed off "…" to @dev`, `Stopped: too many hand-offs`, `@x had nothing to add`), collapsible tool-call cards (tool, arguments, result, duration, the node it ran on, a diff for `edit_file`), streaming text, `NEW` divider, timestamps, Stop while a turn runs | `EntryUpsert`, `TurnUpsert`, `RequestUpsert` |
| Composer | `view/composer.rs` | multi-line input, `@` picker over the group's members, send / stop, a `queued` indicator when the bot is busy elsewhere | `EntryUpsert` on the owner's own message |
| Provider menu | `view/provider_menu.rs` | two columns behind the header's `provider: …` button (DMs only): configured backends left, the selected backend's *self-advertised* models right with a search box; a backend with no advertised list says so and the search box becomes free text. Apply/Clear PATCH `/api/bots/{id}` | `BotUpsert` for the bot |
| Delegation graph | `view/graph.rs` | the tree of work: `handoff_task` / `start_task` nodes with status, hop depth, owner, requester, and the line back to the origin conversation; click to jump | `TaskUpsert` (`parent_task_id`, `requester`, `origin` are the edges) |
| Cards | `view/cards.rs` | approval (Allow once / Always allow / Deny, reason, expiry countdown), question (`ask_user` fields), handoff (Done / Skip) | `RequestUpsert` → `POST /api/requests/:id/respond` |
| Nodes | `view/nodes.rs` | an 8-character pairing code (large, copyable, 10-minute TTL), the node list (label, platform, roots, effective capabilities = declared ∩ approved, ceiling, online), Revoke, the `swarmbot-node pair …` line to copy | `NodeUpsert`, `NodePresence` |
| Settings | `view/settings.rs` | per-bot backend (base url, model, `extra_body` / `extra_headers` for a gateway's own parameters such as an inferhub bid, a masked key field), a **Probe** button showing `BackendStatus` verbatim, safety ceilings, the `bot_messaging` kill switch, `max_hops`, `max_tool_rounds`, the two token caps (`token cap / turn`, `token cap / day`, showing `off` for no cap), the roster | `Snapshot`, settings RPCs |
| Usage | `view/usage.rs` | `GET /api/usage` for a 24h / 7d / 30d window: totals (tokens, model calls, tool calls), a per-bot table with the biggest spender first, and the turn count. Read-only; three states kept apart (`not fetched`, `failed` with a retry, `a report`) | — (REST only; spend is not a live event) |
| Palette | `view/palette.rs` | `ctrl-k`: jump to a bot, conversation or screen | — |

Mapping rules worth stating, because they are where a UI usually lies. The first
two are enforced in `reducer.rs`, not in a view:

* **an entry upsert is not a new line.** Tool calls and streamed replies arrive
  repeatedly under one id at a rising `revision`; `AppState::upsert_entry` keys by
  `EntryId` and replaces. A transcript that appended on every upsert would show a
  bot "saying" its reply eleven times.
* **a card's state comes from `RuntimeRequest.status`**, not from whether the
  button was clicked: the reducer stores the request verbatim and the view reads
  `status`, so an expiry has to look like an expiry.
* **`post: queued` is the owner's only honest signal** that a message is waiting
  because the bot is busy elsewhere; it is rendered on the bubble, not hidden.
* **`EntryKind::Unknown` renders a placeholder.** The contracts carry
  `#[serde(other)] Unknown` precisely so an older UI does not fail on a newer brain
  (D6.6).
* **tool output over 10 KiB is already spilled** by the registry; the card shows
  the capped text and links the spill path rather than pretending to have it all.
* **a `Resync` or a dropped socket is just "apply the next `Snapshot`"** — every
  collection in the state is upserted by id, so recovery needs no special case.

## 4. Widgets GPUI does not ship, and the theme

`gpui` 0.2.2 is the framework, not Zed's component library (`crates/ui` in the Zed
repo is not published). These are hand-built in `crates/ui/src/widgets/`:

* **`text_input.rs`** (874 lines) — caret, selection (`shift-` and select-all),
  clipboard cut/copy/paste through GPUI's platform services, home/end, arrows,
  backspace/delete, multi-line, and its own `actions!(text_input, …)` set so the
  bindings are shared by every field in the app.
* **`markdown.rs`** (502 lines) — `pulldown-cmark` (a dependency,
  `default-features = false`) → GPUI elements: headings, bold/italic, inline code,
  fenced code blocks with a background and a monospace face, lists, and links
  opened with `cx.open_url`. A bot's reply is markdown; rendering it as raw text is
  the difference between a transcript and a wall.
* **`bits.rs`** — the small shared pieces (avatar tile, pill, spinner, section
  header).

Everything else is `div()` + the tailwind-style API, `svg()` for icons and
`uniform_list` for virtualised scrolling.

Theme tokens live in `theme.rs`; the values are the ones the original spike
verified against this compositor, kept so the palette does not drift:

| Token | Value | Used for |
| --- | --- | --- |
| `BACKGROUND` | `#1b1d21` | main pane |
| `SURFACE` / `SURFACE_RAISED` | `#15171a` / `#20242a` | sidebar, raised panels |
| `BORDER` | `#2a2d33` | separators |
| `TEXT` / `TEXT_MUTED` / `TEXT_DIM` | `#e6e6e6` / `#8b9099` / `#6c737d` | primary / secondary / status |
| `ACCENT` / `ACCENT_HOVER` | `#2f6fdb` / `#3d7ff0` | primary button |
| `DANGER` / `SUCCESS` / `WARNING` | `#e5484d` / `#30a46c` / `#f5a524` | deny, allow, pending |
| `OWNER_BUBBLE` / `BOT_BUBBLE` | `#2b3a55` / `#23272e` | transcript bubbles |
| `CODE_BG` | `#111318` | fenced code |
| spacing | `XS 4`, `SM 8`, `MD 12`, `LG 16` | gaps and padding |
| metrics | `SIDEBAR_W 248`, `GRAPH_W 320`, `AVATAR 26`, `HEADER_H 52` | layout |

Bot colours default to the demo team's: `#599ce7` lead, `#a78bfa` scout, `#34d399`
dev, `#fbbf24` sentinel (also their `color` in `config/team.example.toml`).

## 5. Running it

```bash
source scripts/env.sh                       # CARGO_HOME, LIBCLANG_PATH, LIBRARY_PATH, VK_ICD_FILENAMES

cargo run -p swarmbot-ui -- --offline       # spawn a fake-backend brain and open the app
cargo run -p swarmbot-ui -- --selftest      # fabricated events → reducer → render → report → exit
cargo run -p swarmbot-ui -- --selftest --report /tmp/ui-selftest.log
cargo run -p swarmbot-ui -- --brain http://127.0.0.1:8787 --token "$TOK"
cargo build -p swarmbot-ui --features runtime_shaders
```

| Flag | What it does |
| --- | --- |
| `--brain <url>` | the brain to talk to (REST + `/ws/live`). Default `http://127.0.0.1:8787`; **this binary also reads `SWARMBOT_BRAIN`** — the CLI deliberately does not (§7) |
| `--token <tok>` | a one-time login token, the highest-precedence credential source; the type is `SecretToken`, so `Debug` redacts it (§7) |
| `--spawn-brain` | start `swarmbot-brain` as a child, wait for `GET /api/health` to answer `ok` (≤ 20 s), kill it on exit. `std::process` only |
| `--offline` | implies `--spawn-brain` and passes the brain its own `--offline` (scripted `fake` backend) plus `--team` when the demo roster file is present. A missing roster file is reported, not invented |
| `--data-dir <dir>` | data dir for a spawned brain; defaults to a fresh temp dir removed on exit |
| `--brain-addr <addr>` | port for a spawned brain to listen on; defaults to the `--brain` port |
| `--selftest` | drive fabricated `LiveEvent`s through the **real** reducer, render, write a report, exit 0. The machine-checkable proof that a window opens and paints on this compositor |
| `--report <path>` | where the selftest report goes (stdout when omitted) |

`--selftest` exists because an agent cannot look at a window: it turns "does the
render path work on this machine" into an exit code and a log line, and because it
pushes fabricated events through the same reducer the live thread uses, "what
renders" and "what the tests assert on" cannot drift apart. Window creation is
wrapped so a GPUI/blade panic becomes a report line and a non-zero exit instead of
a stack trace nobody reads. `spawn.rs` is equally honest: if the `swarmbot-brain`
binary is not built it returns `SpawnError::NotFound` naming the path it looked at,
and a child counts as started only once `/api/health` answers `ok`.

## 6. Testing

39 tests, none of which needs a compositor, a brain, a model or a network:

* the **reducer** is a pure function (`LiveEvent` + `AppState` → `AppState`), so
  snapshot → upserts → resync, card answers, task-tree edges and connection state
  are asserted on fabricated events;
* **command dispatch** goes through the `Brain` seam (`brain.rs`): the worker's rpc
  thread holds an `Arc<dyn Brain>` and never names `BrainClient` directly, so a
  scripted in-memory double stands in for the whole REST surface. The live
  `/ws/live` socket is deliberately *not* behind that trait — `LiveSubscription` is
  a concrete blocking socket with no in-memory shape — so the live thread is only
  ever exercised against a real or spawned brain, and the reducer that consumes its
  events is tested directly instead;
* **token resolution** is tested for all four sources and for the bounded owner-token
  re-read, with the sleeps injectable so the loop is exercised without waiting (§7);
* **`--selftest`** is the smoke test that needs a real compositor; it is the only
  thing here that opens a window.

## 7. Tokens and credentials

A secret is a **type**, not a `String` (D9.6): `proto::SecretToken` wraps the value,
redacts it in `Debug`, and exposes it through `expose()` at exactly one place — the
wire call (`Authorization: Bearer`, `POST /api/session`). The same discipline
appears as hand-written redacting `Debug` impls in `llm`, `client` and `brain`.
Tests assert the bytes appear in no `{:?}` and in no report.

Resolution order, highest first:

1. `--token`;
2. `SWARMBOT_TOKEN`;
3. `<data-dir>/owner-token` — the one-time owner token the brain mints at boot;
4. nothing: the window opens unauthenticated and says so.

A whitespace-only `--token`/`SWARMBOT_TOKEN` counts as absent, and so does an
empty, missing or unreadable owner-token file: every failed source falls through to
the next rather than producing an empty bearer header.

Step 3 has one wrinkle, and it is the reason `--offline` works at all (D9.10): when
this process spawned the brain, the token file is being written *right now*, so that
one case gets a short bounded re-read — **4 attempts, 120 ms apart** (~360 ms worst
case) — and no other case retries at all. The alternative was asking the brain to
exempt `/api/health` from auth; instead **no endpoint is unauthenticated** and the
demo still needs no credentials.

**`SWARMBOT_BRAIN` is read by this binary and not by the CLI.** That is deliberate:
`swarmbot` has two modes (embedded and `--brain`) and an inherited environment
variable would silently switch between them, so the CLI takes the URL only as a
flag. The UI has one mode — it is always a client — so a default from the
environment is a convenience there and a hazard here.

## 8. Actions and key bindings

`actions.rs` declares two action sets and binds them once at launch, before the
first window. App-level actions are `Quit`, `ToggleGraph`, `TogglePalette`, `Send`,
`InsertNewline`, `CancelOverlay`, `SelectNext`/`SelectPrev`/`SelectConfirm`,
`FocusComposer`, `StopTurn`; `text_input` has its own editing set so every field
shares one keymap.

| Binding | Context | Action |
| --- | --- | --- |
| `ctrl-q` | global | quit |
| `ctrl-g` | global | toggle the delegation graph |
| `ctrl-k` | global | toggle the palette |
| `escape` | root only | close the palette, else the mention picker. Bound at the root because it is the last element on the focus path, so it always gets the event |
| `enter` / `shift-enter` | `Composer` | send / newline |
| `up` / `down` | `Composer` | mention-picker navigation when it is open; the composer forwards them to the caret when it is not |
| `up`/`down`/`enter`/`ctrl-n`/`ctrl-p` | `Palette` | palette navigation |
| arrows, `backspace`, `delete`, `home`, `end`, `ctrl-a/c/x/v`, `shift-`selection | global (only `TextInput` listens) | editing; bound for both the Linux `ctrl-` and the macOS `cmd-` modifiers |

Scoped bindings (`Some("Composer")`, `Some("Palette")`) beat the global editing ones
only inside those elements, which is how `enter` can mean "send" in the composer and
"insert a newline" in a multi-line card field.

## 9. State and the thread contract

* `state.rs` — `AppState`: plain data, every collection keyed by id, plus
  `ConnectionState`. No GPUI types, so it can be built and asserted in a test.
* `reducer.rs` — `apply(state, event)`: the only place state changes. Pure, no I/O,
  no clock reads that matter.
* `proto.rs` — `Command` (main thread → rpc thread), `WorkerMsg` (workers → main
  thread: `CommandResult`, `ConnEvent`, live events), and `SecretToken`. The values
  that cross the thread boundary are data, not handles.
* `brain.rs` — the `Brain` seam over `BrainClient`, with a scripted double for
  tests (§6).
* `worker.rs` — the two threads, the backoff, and the main-thread pump (§1).

The contract in one line: **the workers produce data, the reducer folds data into
state, and only the main thread touches GPUI.** Anything that needs to change a
view goes through that path, which is why a resync, a reconnect and a cold start
are the same code.

## 10. What is not verified

* **What the pixels look like.** The windowing half is verified: on 2026-11-14,
  `--selftest` opened 11 windows, rendered every screen, and exited 0 on the
  NVIDIA ICD (§2). The number of render calls it reports differs per run (frames
  are timing-dependent), so it is evidence that painting happened, not a figure
  to quote. What that does **not** prove is that any of it
  looks right — and on 2026-11-14 the desktop session was locked, so a screenshot
  captured the lock screen rather than the app. Read `selftest: window N drew
  screen=…` as "a root view rendered", never as "this screen is correct".
* **Frame rate.** Nobody has measured frames per second, on this GPU or under a
  software rasterizer, with a long transcript on screen.
* **A real brain round trip from this app.** The tests use the scripted `Brain`
  double; `--offline --spawn-brain` exercises the real client against a real brain,
  but only by hand. It does work: a 2026-11-14 run printed `brain reachable and
  authenticated (version 0.1.0, epoch 1)` and `GET /api/bots` served the
  `config/team.waypoint.toml` roster through it.
* **The tty token prompt** (the CLI's, not this app's) needs a real terminal; the
  UI has no prompt at all — it reads the four sources in §7 and says so when none
  produced a token.
* **macOS and Windows.** The `wayland` feature set is Linux-only as pinned; macOS
  would need `macos-blade` and Windows `windows-manifest`, neither of which is
  enabled or tested here.

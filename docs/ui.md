# SwarmBot desktop UI (GPUI)

`crates/ui`, binary `swarmbot-ui`. A native desktop app that watches bots
cooperate: the roster, the transcripts, the delegation graph, and the cards that
need a human.

**Current state: the app is built.** 18,162 lines across 32 source files, **163
tests**, one window with a sidebar, a transcript, a composer, a delegation-graph
panel, cards, a nodes screen, a routines screen, a usage screen, a settings screen,
an **approval-rules** screen and a command palette. It is a pure client of the brain's REST + `/ws/live` API, so
it needs a brain to talk to — and `--offline --spawn-brain` starts one with the
scripted `fake` backend, which is a genuine zero-credential demo (§5, §7). **The
`fake` backend has no model in it**: with nothing scripted, every reply is the
literal string `[fake] no scripted turn left`. A real conversation needs
`--brain <url>` pointed at a `swarmbot-brain` whose config names a real backend —
§10.2 records one such pass, done through the CLI because the desktop session
locked before a message could be typed into this window.

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
| **A session with no GPU at all still renders (2026-11-15).** Twice in one day — at ~08:24 and again at ~09:36 — this box had no `/dev/nvidia0`, no `/dev/dri` and an `nvidia-smi` that could not reach the driver, with the GPU *present* in between; `scripts/env.sh` exported the staged lavapipe ICD, and `./target/debug/swarmbot-ui --selftest` opened **12 windows**, `ok=true`, exit 0, with `vulkan/renderer: icd=…/.tmp-ui-gpu/lvp_local.json`. The whole 2026-11-15 pixel pass (§10.1) ran on this path | The D9.34 fallback, exercised for real rather than in a namespace. It proves the CPU path works end to end and says nothing about the hardware — and the twice-in-one-session flip is why the sibling row needs a date |
| **The GPU works (re-checked 2026-11-14).** `/dev/nvidia0`, `/dev/nvidiactl`, `/dev/nvidia-modeset`, `/dev/nvidia-uvm`, `/dev/dri/card1` and `/dev/dri/renderD128` all exist; `nvidia-smi` reports a GTX 1660 SUPER on driver 615.78.08; `nvidia_icd.json` is the ICD in use | `./target/debug/swarmbot-ui --selftest` **opens 12 windows and exits 0**, with `vulkan/renderer: icd=/usr/share/vulkan/icd.d/nvidia_icd.json` (11 before the Rules screen existed; the count moves with the screens). The *render total* it prints is not stable — 22, 54, 55, 57 and 58 across five runs of one binary — so assert on the 11 windows and on `ok=true`, never on a render count. The 2026-11-13 entry here said the opposite (`NoSupportedDeviceFound` panic, no usable ICD) and that was true then; it is not true now. `scripts/env.sh` still prefers llvmpipe *when that ICD exists* and still warns on stderr when none does — a headless-box fallback, not a statement about this machine |
| **A session can also see no GPU at all** — a sandbox whose `/dev` has only `null`/`zero`/`tty`, CI, a headless box. There is no render node and no system llvmpipe, so GPUI's own init panics (`Unable to init GPU context: NoSupportedDeviceFound`). Note the weaker check that looks right and is not: `test -e /dev/dri` answers **yes** for an empty bind-mounted directory, and a zero-length `/dev/nvidia0` is a placeholder — both measured on 2026-11-14 | `scripts/software-vulkan.sh` stages Mesa lavapipe **inside the checkout** (`.tmp-ui-gpu/`, gitignored) with no root: it downloads the distro package with `pacman -Sp` + `curl`, extracts it with `bsdtar`, and writes `.tmp-ui-gpu/lvp_local.json` naming the library by **absolute** path (so no `LD_LIBRARY_PATH`). `scripts/env.sh` then exports `VK_ICD_FILENAMES` to it, and `--selftest` **opens the same windows and exits 0** through the CPU renderer — proven twice: in a mount namespace with a blanked `/dev` on 2026-11-14 (`opened 11 window(s)`), and again on 2026-11-15 in a session whose host genuinely had no GPU (`opened 12 window(s)` after the Rules screen was added). Both report `vulkan/renderer: icd=…/.tmp-ui-gpu/lvp_local.json` and `ok=true` |
| `pkg-config vulkan` missing, `libvulkan.so.1` present | enough: blade loads the Vulkan library at runtime |
| the crate has a `runtime_shaders` feature (off by default) | enables it for shader iteration without a build-time shader toolchain; leave it off for normal builds |
| `swarmbot-ui` is **not** in the workspace `default-members` | plain `cargo build`/`cargo test` skip it (its dependency chain is minutes long); `--workspace`, `scripts/check.sh` include it, `scripts/check.sh --fast` excludes it again |

## 3. Screens

One window: a left sidebar (`SIDEBAR_W = 280 px`), a main pane, and a toggleable
delegation-graph panel on the right (`GRAPH_W = 320 px`). Light theme (`theme.rs`);
no xAI/Grok
fonts, logos, icons or avatar art (D0.3) — an avatar is a tile in the bot's colour
with its initial, and the colour comes from `Bot::effective_color` (FNV-1a over the
id → hue → `#rrggbb`) unless the roster names one.

| Screen | Module | Shows | Live events that drive it |
| --- | --- | --- | --- |
| Root layout | `view/mod.rs` | sidebar + main pane + graph panel, the header, overlays | — |
| Sidebar | `view/sidebar.rs` | bots (avatar tile, label, status line, activity), conversations (DM and group, preview, unread), nodes (online/offline, capabilities), settings | `BotUpsert`, `BotStatus`, `ConversationUpsert`, `NodeUpsert`, `NodePresence` |
| Conversation | `view/conversation.rs` | transcript: **owner bubbles right, bot bubbles left**, event lines (`Messaged @scout`, `Handed off "…" to @dev`, `Stopped: too many hand-offs`, `@x had nothing to add`), collapsible tool-call cards (tool, arguments, result, duration, the node it ran on, a diff for `edit_file`), streaming text, `NEW` divider, timestamps, Stop while a turn runs | `EntryUpsert`, `TurnUpsert`, `RequestUpsert` |
| Composer | `view/composer.rs` | multi-line input, `@` picker over the group's members, send / stop, a `queued` indicator when the bot is busy elsewhere | `EntryUpsert` on the owner's own message |
| Provider menu | `view/provider_menu.rs` | two columns behind the header's `provider: …` button (DMs only): configured backends left with a per-provider connection dot, the selected backend's *self-advertised* models right with a search box; a backend with no advertised list says so and the search box becomes free text; below both, a **CONNECTION** strip: the state, the brain's `detail`, **Connect**, and one **Log in** button per login the agent advertises. A successful Connect also refetches the models, so the model column fills without a second click. Apply/Clear PATCH `/api/bots/{id}` | `BotUpsert` for the bot, `Connected` |
| Delegation graph | `view/graph.rs` | the tree of work: `handoff_task` / `start_task` nodes with status, hop depth, owner, requester, and the line back to the origin conversation; click to jump | `TaskUpsert` (`parent_task_id`, `requester`, `origin` are the edges) |
| Cards | `view/cards.rs` | approval (Allow once / Always allow / Deny, reason, expiry countdown), question (`ask_user` fields), handoff (Done / Skip) | `RequestUpsert` → `POST /api/requests/:id/respond` |
| Nodes | `view/nodes.rs` | an 8-character pairing code (large, copyable, 10-minute TTL), the node list (label, platform, roots, effective capabilities = declared ∩ approved, ceiling, online), Revoke, the `swarmbot-node pair …` line to copy | `NodeUpsert`, `NodePresence` |
| Settings | `view/settings.rs` | per-backend cards: the redacted config, a **connection state** (`not checked` / `connected` / `needs a login` / `not connected`) with a **Connect** button per backend, the logins that backend advertises as buttons plus the exact login command with **Copy**, and the brain's own `detail` verbatim; a global **Probe**; safety ceilings, the `bot_messaging` kill switch, `max_hops`, `max_tool_rounds`, the two token caps (`token cap / turn`, `token cap / day`, showing `off` for no cap), the roster | `Snapshot`, settings RPCs, `Connected` |
| Approval rules | `view/rules.rs` | every saved `ApprovalRule`: scope (bot/surface/tool), the matcher, the decision, who saved it and when, with Delete. A rule with **no matcher** is called out in red, because it decides every request in its scope — the hazard `routes/rules.rs` warns about and that used to be visible only in SQLite | `Snapshot` (the rules travel with it), `FetchRules` |
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
* **`connected` means the handshake passed, and the card says so.** A backend row
  whose handshake is green *and* that advertises a login carries one dim line
  naming that login and the condition it matters under ("if a turn is refused
  with an authentication error, run it and press Re-check") — not a prediction:
  `opencode` advertises a login and answers turns, `claude-code-acp` advertises
  one and refuses its first prompt (D9.28, D9.33). The login buttons carry the
  agent's own method ids — an id nobody advertised is never sent — and a login
  the agent cannot perform is displayed in the agent's words. A backend that
  already answered offers **Re-check**, not "Connect": the action is the same
  handshake and the label says so.
* **a list that was never fetched, a list that failed, and an empty list are
  three different screens.** `state.rs` holds `Fetched<T> { Unknown, Failed(reason),
  Ready(T) }` for bots, conversations, nodes, routines and approval rules, because
  a cold start or a dead brain used to render the same "nothing here yet" as a
  genuinely empty installation. Only an answer *that was actually read* may say
  empty; a live event does not promote `Failed` to `Ready` (one row is not a list).
* **a snapshot merges, it does not replace.** Entries are merged by `EntryId` with
  the existing revision guard, so a resync no longer deletes the older pages the
  owner loaded with "Load earlier"; entries for a conversation the snapshot no
  longer lists are still dropped.
* **a timestamp names its day.** `time.rs` formats against the viewer's calendar
  day ("Today 5:34 PM", "Yesterday 9:02 AM", "Nov 3, 5:34 PM"), with `now` and the
  zone injected so the boundaries are testable; the transcript's message grouping
  also breaks at midnight.
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

Theme tokens live in `theme.rs`. **The app ships a light theme now** — the dark
palette below is what the original spike used and what this table used to claim
long after the code moved on (D9.35). The table lists the values in the source:

| Token | Value | Contrast | Used for |
| --- | --- | --- | --- |
| `BACKGROUND` | `#ffffff` | — | main pane |
| `SURFACE` / `SURFACE_RAISED` | `#f7f7f8` / `#ececee` | — | sidebar, raised panels |
| `BORDER` | `#e5e7eb` | — | separators |
| `TEXT` | `#111827` | 16.0:1 | primary text |
| `TEXT_MUTED` | `#4b5563` | 7.6:1 | secondary text |
| `TEXT_DIM` | `#5f6976` | 5.6:1 (4.7:1 on a raised card) | timestamps, metadata, section labels |
| `ACCENT` / `ACCENT_HOVER` | `#1d4ed8` / `#1e3a8a` | 6.7:1 | primary button, links |
| `DANGER` / `SUCCESS` / `WARNING` | `#ef4444` / `#10b981` / `#f59e0b` | fill only | status dots, button faces |
| `DANGER_TEXT` / `SUCCESS_TEXT` / `WARNING_TEXT` | `#b91c1c` / `#047857` / `#92400e` | 5.5:1 / 7.1:1 / 5.9:1 | the word beside the dot |
| `OWNER_BUBBLE` / `BOT_BUBBLE` | `#000000` / `#f3f4f6` | — | transcript bubbles |
| `CODE_BG` | `#f3f4f6` | — | fenced code |
| notice | `#fef2f2` bg / `#fecdd3` border / `#991b1b` fg | — | error banner |
| spacing | `XS 4`, `SM 8`, `MD 12`, `LG 16` | — | gaps and padding |
| metrics | `SIDEBAR_W 280`, `GRAPH_W 320`, `AVATAR 32`, `HEADER_H 56` | — | layout |

**Every text token is a measured 4.5:1, and the test is what keeps it there.**
The row above used to read `TEXT / TEXT_MUTED / TEXT_DIM = #111827 / #6b7280 /
#9ca3af`, and that last value is **2.5:1 on white** — the colour of every
timestamp, preview line and section heading in the app. WCAG AA wants 4.5:1 for
body text, so the three tiers were re-cut: `#6b7280` became the *faintest* tier
and had to darken to `#5f6976` to clear 4.5:1 on the darkest surface it is drawn
on, and the secondary tier moved to `#4b5563` to stay visibly darker than it.
`theme::tests::every_text_token_is_legible_on_its_surface` recomputes every
ratio from the constants on every `cargo test`, so a future palette edit that
drops under the bar is a red test rather than a screenshot nobody takes.

**The status hues are split in two, on purpose.** A hue you can see as a 9px dot
and a hue you can read as a 12px word are different colours: green `#10b981` is
2.1:1 on white, amber `#f59e0b` is 1.8:1. So `DANGER`/`SUCCESS`/`WARNING` are
*fill* tokens and the `*_TEXT` twins are the text ones, and
`status_fills_are_visible_even_though_they_are_not_body_text` asserts both halves
of that split — including that the fill has *not* been darkened into its twin.
The same rule retired one more pattern: a bot's name is no longer drawn in its
roster colour (the shipped colours are 1.5–2.6:1 on the bubble's grey), it is
`TEXT` with a small colour dot beside it and the avatar tile still carrying the
hue.

**Rows are sized by shared builders, not by literals.** `bits::screen_header`
draws the one title row every non-conversation screen wears, at `HEADER_H`, with
the same padding and divider — those five screens had grown to ~39px while the
conversation header was 56px, so switching screens shifted the whole page by 17px.
`bits::kv` gives its label column one width (`KV_LABEL_W`) for the same reason:
120px was narrower than the labels the app actually uses.

The module header of `theme.rs` said "dark-first (the only variant the app ships)"
while every token was light. That comment is the thing that was wrong, not the
pixels: it is corrected to match, and this table with it.

Bot colours default to the demo team's: `#599ce7` lead, `#a78bfa` scout, `#34d399`
dev, `#fbbf24` sentinel (also their `color` in `config/team.example.toml`).

## 5. Running it

```bash
source scripts/env.sh                       # CARGO_HOME, LIBCLANG_PATH, LIBRARY_PATH, VK_ICD_FILENAMES

cargo run -p swarmbot-ui -- --offline       # spawn a fake-backend brain and open the app
cargo run -p swarmbot-ui -- --selftest      # fabricated events → reducer → render → report → exit
cargo run -p swarmbot-ui -- --selftest --report /tmp/ui-selftest.log
cargo run -p swarmbot-ui -- --brain http://127.0.0.1:8787 --token "$TOK"
# real models on this box: point it at the roster that has them. It discovers
# `config/swarmbot.toml` (opencode, model pinned) by itself and reads the owner
# token from `<data-dir>/owner-token`, so no `--token` is needed.
cargo run -p swarmbot-ui -- --data-dir ./data
cargo build -p swarmbot-ui --features runtime_shaders
```

| Flag | What it does |
| --- | --- |
| `--brain <url>` | the brain to talk to (REST + `/ws/live`). Default `http://127.0.0.1:8787`; **this binary also reads `SWARMBOT_BRAIN`** — the CLI deliberately does not (§7) |
| `--token <tok>` | a one-time login token, the highest-precedence credential source; the type is `SecretToken`, so `Debug` redacts it (§7) |
| `--spawn-brain` | start `swarmbot-brain` as a child, wait for `GET /api/health` to answer `ok` (≤ 20 s), kill it on exit. `std::process` only |
| `--offline` | implies `--spawn-brain` and passes the brain its own `--offline` (scripted `fake` backend) plus `--team` when the demo roster file is present. A missing roster file is reported, not invented |
| `--data-dir <dir>` | data dir for a spawned brain; defaults to a fresh temp dir removed on exit. **Use `./data` to get the real roster**: the default `$XDG_DATA_HOME/swarmbot` on this machine has a database but no bots, and a brain given an existing database gets no `--team`, so the app would open an empty roster (see D9.28) |
| `--brain-addr <addr>` | port for a spawned brain to listen on; defaults to the `--brain` port |
| `--selftest` | drive fabricated `LiveEvent`s through the **real** reducer, render, write a report, exit 0. The machine-checkable proof that a window opens and paints on this compositor |
| `--report <path>` | where the selftest report goes (stdout when omitted) |

**A machine with no Vulkan driver says so instead of panicking.** `gpu.rs` decides
before GPUI does, and its rule is narrow on purpose: a driver is proven present by a
configured `VK_ICD_FILENAMES`/`VK_DRIVER_FILES` (the caller chose), a **render node**
inside `/dev/dri` (`renderD*`/`card*`) or a device `/dev/nvidia0`, or a *software*
ICD the loader scans by itself (`/usr/share/vulkan/icd.d/lvp_icd*.json`). Presence is
checked by contents, not by `test -e`: an empty `/dev/dri` and a zero-length
`/dev/nvidia0` both pass the weak check and then panic, which is the failure being
prevented. Only when all three miss does the run print one actionable sentence
(`vulkan/driver: no Vulkan driver is reachable`, then the script and
`source scripts/env.sh`), write `exit: renders=0 … ok=false`, and exit **2** — not
1, because "this machine cannot open a window" is not "the app failed". Everything
else (a brain, a config, a token) works without a driver, so nothing is disabled;
the window is simply the one thing that cannot happen. The staged ICD is
deliberately *not* counted as "software present": the loader never scans inside a
checkout, so it becomes usable exactly when `env.sh` exports it — and that is the
`Configured` case.

```bash
scripts/software-vulkan.sh        # no root: stages lavapipe inside the checkout
source scripts/env.sh             # picks it up (hardware still wins when present)
cargo run -p swarmbot-ui -- --selftest
```

`--selftest` exists because an agent cannot look at a window: it turns "does the
render path work on this machine" into an exit code and a log line, and because it
pushes fabricated events through the same reducer the live thread uses, "what
renders" and "what the tests assert on" cannot drift apart. Window creation is
wrapped so a GPUI/blade panic becomes a report line and a non-zero exit instead of
a stack trace nobody reads. `spawn.rs` is equally honest: if the `swarmbot-brain`
binary is not built it returns `SpawnError::NotFound` naming the path it looked at,
and a child counts as started only once `/api/health` answers `ok`.

## 6. Testing

163 tests, none of which needs a compositor, a brain, a model or a network — the
number grew by 40 when the 2026-11-15 pixel pass added a test to every defect it
fixed (§10.1):

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
* **driver detection** is a pure table (`gpu.rs`): configured environment, device
  node, software ICD, absent — including the case that matters, an `nvidia_icd.json`
  sitting on the box whose device is not (a naive "any ICD file exists" check would
  pass there and then panic);
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

### 9.1 The seam the brain grew in the honesty pass (D9.35)

The client gained routes this screen set does **not** use yet. They are listed so
nobody reads their absence as a bug:

| Client method | Route | In the UI? |
| --- | --- | --- |
| `metrics()` | `GET /api/metrics` | no — `swarmbot status` (CLI) reads it |
| `secrets()` / `set_secret()` / `delete_secret()` | `/api/secrets` | no — `swarmbot config key` (CLI) |
| `export_transcript()` | `GET …/export` | no — `swarmbot transcript --out` (CLI) |
| `update_node()` | `PATCH /api/nodes/{id}` | no — the Nodes screen still only pairs and revokes |
| `entries_before()` | `GET …/entries?before_seq=` | **yes** — the "Load earlier" row |
| `rules()` / `delete_rule()` | `/api/rules` | **yes** — the Approval rules screen |

The gap is deliberate rather than forgotten: each of those is a *terminal* action
(inspect, copy a secret in, export a file) where the CLI is the cheaper surface, and
the two that change what the transcript shows are in the UI. A future screen that
needs them has a client method waiting.

## 10. What is not verified

* **Clicking and dragging.** The pixels were looked at on 2026-11-15 (see §10.1
  for how, and what was found), but every interaction in that pass was **driven
  from the keyboard**: `wtype` sends keys and `grim` captures, and neither can
  press a button at a coordinate. Every `.on_click` handler, every hover state,
  the delegation graph's layout and the `@`-mention picker's mouse behaviour are
  still unexercised on a real compositor. A screen can look right and still have
  a click target in the wrong place.
* **Frame rate.** Nobody has measured frames per second, on this GPU or under a
  software rasterizer, with a long transcript on screen.

### 10.1 The 2026-11-15 pixel pass, and what it changed

The debt this section used to record ("nobody has looked at the pixels") was paid
by looking. The method is repeatable and needs no root, no extra package and no
test-only feature:

```bash
source scripts/env.sh
./target/debug/swarmbot-ui --offline --spawn-brain &   # a real brain, a real window
sleep 8
hyprctl clients -j | grep -o '0x[0-9a-f]*'            # find the window's address
grim -g "12,38 1896x1030" shot.png                    # capture it, pixel for pixel
wtype -M ctrl -k k -m ctrl ; wtype "usage" ; wtype -k Return   # drive it
```

`--offline --spawn-brain` is what makes this honest: the screen under review is
fed by the real client and a real brain, not by `--selftest`'s fixtures. The
fixtures are still useful for a screen no brain state provides, but a review that
only ever saw them would not know what the app looks like when it is running.

Four defects were found by *looking*, and none of them were reachable from
`--selftest` — every one of them passed `window N drew screen=…`:

1. **The keyboard died after every palette jump.** `ctrl-k`, type, `Enter` moved
   to the screen and then no keystroke ever arrived again — `ctrl-k` included, so
   the app looked frozen while it kept painting. Cause: GPUI resolves a keystroke
   against the *rendered* frame, and the element that had focus (the palette's
   search field, or the composer) is not in a screen that does not draw it. The
   fix is [`Root::focus_seat`](../crates/ui/src/view/mod.rs): a render-time check
   of `(screen, palette_open, needs_login)` that re-seats focus on an element the
   new surface actually renders.
2. **The palette reopened with the previous query still typed.** A second
   `ctrl-k` + typing produced `usagenodes` and an empty result list — a search box
   with text in it that nobody had typed this time. [`Palette::reset`] clears it
   on every open.
3. **Two clocks in one transcript row.** The separator above a message said
   `Today 8:41 AM` and the bubble's own header said `01:41` for the same instant,
   because the header formatted the UTC value directly. Both now go through
   [`crate::time`]; `time::clock_label` and a test pin the agreement.
4. **Text was unreadable, and a static audit put numbers on it.** `TEXT_DIM`
   (`#9ca3af`) was 2.5:1 on white and was the colour of every timestamp, section
   heading and preview line; the vivid status hues were used as *text* (green at
   2.1:1, amber at 1.8:1); the author's name was drawn in the bot's roster colour
   (`#fbbf24` is 1.5:1 on the bubble). §4 records the tokens that replaced them,
   and `theme::tests::every_text_token_is_legible_on_its_surface` computes every
   ratio from the constants, so the palette cannot drift back under 4.5:1.

`--selftest` itself got stricter in the same pass, because "a window opened" was
never the same claim as "the screen painted": it now counts renders **per
window** and fails the run if one opened without drawing, and it compares the
window count against `selftest::WINDOWS` instead of reporting whatever it opened.
A failing screen now makes the process exit non-zero; before, a `WINDOW FAILED`
line was printed and the run still exited 0 as long as some other window painted.
The report line reads `opened 12 window(s), 12 of them painted`, and that
distinction is the point: the number is now a count of *screens that drew*, not of
windows that exist.

**The fixes were re-captured, not just tested.** The four defects above were found
by looking at the app, and after the fix pass the same eight screens were captured
again on an unlocked session (`--offline --spawn-brain`, one window, `grim` on the
window's own geometry). Four of them are visible in those captures and are worth
naming, because each one *was* wrong in the before-shot:

* **the sidebar preview now ends in `…`** — `Group created with @wayp…`, where the
  before-shot cut it at `…@waypoint, @` with no ellipsis at all;
* **one clock per instant** — the separator reads `Today 9:27 AM` and the bot
  bubble's own header reads `9:27 AM`, where it used to read `02:27` (UTC) under a
  local separator;
* **the author's name is `TEXT` with a 6px colour dot beside it**, where it used to
  be drawn in the roster hue at 1.5–2.6:1;
* **the palette reopens empty** — `Jump to a bot, conversation or task…` with the
  full unfiltered list, where the before-shot showed the previous query still in
  the box.

The session locked at ~09:17 during the first attempt and unlocked again at ~09:22;
the timeout on `grim` was the symptom (it blocks rather than failing when DPMS is
off), which is worth knowing: **`grim` hanging is a sign the screen is asleep, not
a broken capture**.

The captures for this pass are committed under [`pixel-pass/`](pixel-pass/):
[transcript](pixel-pass/2026-11-15-transcript.png),
[nodes](pixel-pass/2026-11-15-nodes.png),
[usage](pixel-pass/2026-11-15-usage.png),
[settings](pixel-pass/2026-11-15-settings.png),
[rules](pixel-pass/2026-11-15-rules.png) and
[palette](pixel-pass/2026-11-15-palette.png). They are evidence for a dated
build, not a style guide: a later screenshot that disagrees with them is newer,
not wrong. Delete them without ceremony once a pass re-captures the set.

The eight captures cover: Conversation (DM, and a group), Usage, Nodes, Routines,
Settings, Rules, the delegation-graph panel, and the palette overlay over a
transcript. Two of them are worth reading as a pair — the Nodes screen shows
`declared / approved` sitting under `capabilities` on a shared 150px label column
with no collision, and Settings/Usage/Routines/Rules all open at the same 56px
title row, which is what `bits::screen_header` was for.

Still not proved by a screenshot, and stated so rather than implied:
`EntryKind::Reasoning`/`Notice` collapse states, the `@`-mention picker, the
create-bot / create-group modals (they exist in `--selftest`'s window set but not
in the committed captures), a long tool-call card with a real node id, the
provider menu's two columns with a real backend row, and **every width but one** —
the whole pass ran in a tiled 1896×1030 window, so nothing here says how the
three-column layout behaves at 900px or how the sidebar behaves at 600.

### 10.2 A real chat pass (2026-11-15), and the defect it found

The chat path was exercised end to end against a **real brain process and a real
ACP agent**, not the scripted `fake`:

```bash
# a real brain: shipped config (opencode as default_backend), a throwaway data dir
swarmbot-brain --config .scratch/real/config.toml --team config/team.waypoint.toml
# a real turn, through the CLI against that brain
swarmbot chat waypoint --brain http://127.0.0.1:8787 --data-dir .scratch/real/data
```

Four turns completed: a direct DM to `@waypoint`, a DM to `@alfred` that used
`read`/`glob`/`grep` in its own workspace (and said plainly that `MEMORY.md` was
empty rather than inventing a schedule), a group message that woke `@alfred` by
mention, and a `handoff_task` from `@waypoint` to `@parcel` that really created
`tsk_…` and really delivered its result back. Afterwards `turns_running` was 0 and
no delivery was left queued — the bookkeeping matches the work.

**The defect it found: a task settled `succeeded` with `result: ""`.** The
`@parcel` handoff needed `shell`, the node's permission boundary refused it
(`ACP permission decided … decision=select(reject: Reject)` in the brain log), the
agent ended its own turn having written no answer — and the task closed as
**succeeded with an empty result**. The requester could not tell "finished, nothing
to report" from "produced nothing", so `@waypoint` spent a whole extra turn asking
*"your result for the task … reached me empty"*, and `@parcel` spent another turn
re-reading its workspace. One blank string cost two model turns.

The fix is `core::runners::task_result`: the status stays `succeeded` (the turn
really did end normally, and calling it a failure would be its own lie), but the
result now names what happened — "the bot finished its turn without writing a
result — it may have only used tools, or the tool it needed was refused", or the
`NO_REPLY` case in its own words. Three tests pin it, including one that the
pass-through of a real answer and a real error did *not* change. Re-running the
same handoff against the fixed brain produced exactly that sentence in
`GET /api/tasks`.

**Also measured, and left alone on purpose**: `usage.tool_calls` counted `3`
across seven turns where the transcripts show far more agent tool activity. That
is by design — `runners.rs` counts the tool calls the runner executed or the
*team* tools an agent invoked through our MCP gateway; an ACP agent's own
``read``/``grep``/``execute`` run in the agent's sandbox and are transcribed as
`origin: Agent`, so counting them as our tool calls would overstate what SwarmBot
did. The UI and `swarmbot usage` both say "tool calls", which is now the one
sentence that should be tightened if the owner wants that number to mean "all
activity".

**Not tested here**: the UI doing this (§10, the session locked), token usage (an
ACP agent reports no per-turn tokens, so `input_tokens`/`output_tokens` are 0 —
D9.28's known limit), and the `@`-mention picker or clicking a card, which need a
mouse.


* **Frame rate.** Nobody has measured frames per second, on this GPU or under a
  software rasterizer, with a long transcript on screen.
* **A real brain round trip from this app.** The tests use the scripted `Brain`
  double; `--offline --spawn-brain` exercises the real client against a real brain,
  but only by hand. It does work: a 2026-11-14 run printed `brain reachable and
  authenticated (version 0.1.0, epoch 1)` and `GET /api/bots` served the
  `config/team.waypoint.toml` roster through it.
  **A real *chat turn* was driven on 2026-11-15, but through the CLI, not this
  window** (§10.2): the window was launched against a real brain with a real ACP
  agent and rendered, `exit: renders=1119 ... ok=true`, and then the desktop
  session locked before a message could be typed into it. So "the UI can complete
  a turn with a real model" is still unproven; "the client and brain underneath it
  can" is now proven with four real turns, one of them a bot-to-bot handoff.
* **The tty token prompt** (the CLI's, not this app's) needs a real terminal; the
  UI has no prompt at all — it reads the four sources in §7 and says so when none
  produced a token.
* **macOS and Windows.** The `wayland` feature set is Linux-only as pinned; macOS
  would need `macos-blade` and Windows `windows-manifest`, neither of which is
  enabled or tested here.

# SwarmBot

Tim bot bernama yang saling bekerja sama — **murni Rust**. Setiap bot adalah satu
*pikiran* (satu sesi model) dengan persona dan pekerjaannya sendiri: mereka
mengobrol 1:1 dengan pemilik, berdiskusi di grup, saling mengirim pesan, saling
menyerahkan tugas (`handoff_task`), meminta persetujuan sebelum tindakan berisiko,
dan bekerja di mesin lewat tool (`shell`, `read_file`, `edit_file`, …). Satu
proses **brain** menjalankan orkestrasi, pemanggilan model, penyimpanan, dan API;
proses **node** terpisah menjalankan tool di mesin lain; antarmukanya CLI dan
aplikasi desktop **GPUI**. Tidak ada Node.js, Bun, atau TypeScript saat runtime.

Keputusan desain yang mengikat ada di [`docs/decisions.md`](docs/decisions.md) —
baca itu dulu; dokumen ini hanya pintu masuk.

## Topologi

```
 pemilik
   │
   ├── swarmbot      (CLI)  ──┐
   └── swarmbot-ui   (GPUI) ──┤  REST + /ws/live  (loopback 127.0.0.1:8787)
                              ▼
                     ┌──────────────────────────────────────────────────────┐
                     │ swarmbot-brain                              (tokio)  │
                     │  core    RunnerMap · Orchestrator · Messaging        │
                     │          BackgroundTasks · ApprovalService           │
                     │          LiveHub · Transcript · InstructionRenderer  │
                     │  llm     OpenAiCompat · ClaudeCli · Fake             │
                     │  tools   ToolRegistry + 7 handler + PathPolicy       │
                     │  store   SQLite (WAL) + transcripts/*.jsonl          │
                     └───────────────┬──────────────────────────────────────┘
                                     │ node lokal: in-process, `nod_local`
                                     │ (NodeExecutor yang sama, tanpa socket)
                                     ▼
                     ┌──────────────────────────────────────────────────────┐
                     │ swarmbot-node …   mesin lain, mendial ke brain       │
                     │  ws://<brain>/node · Ed25519 · policy sendiri        │
                     └──────────────────────────────────────────────────────┘
```

UI dan CLI **selalu** berbicara lewat API publik brain, tidak pernah menyemat
orkestrasi di dalamnya — kecuali `swarmbot` tanpa `--brain`, yang membangun `Swarm`
yang sama di proses sendiri (mode `embedded`; kenyamanan, bukan jalur kode kedua:
satu port `Session` menutupi keduanya). Karena itu sebuah UI bisa menunjuk brain di
mesin lain.

Sisi node pada diagram nyata di kedua ujungnya: `crates/node` (daemon, 135 tes) dan
`crates/brain/src/gateway/` (pairing, upgrade bertanda tangan, roster, dispatcher).
`crates/node/tests/real_brain.rs` menjalankan keduanya bersamaan — brain in-process
plus **binary `swarmbot-node` sungguhan** sebagai child process — dan membuktikan
tool berjalan di node, bukan di brain, lewat berkas penanda di root node dan umpan
palsu di workspace brain.

## Prasyarat

| Kebutuhan | Keterangan |
| --- | --- |
| Rust stable | `rust-toolchain.toml` memilihnya; edition 2024, `rust-version = 1.90`, komponen `rustfmt` + `clippy` |
| `cmake`, `clang`/`libclang`, `pkg-config` | build script `gpui` menjalankan `bindgen` dan `cbindgen` |
| Wayland + `libwayland-client/cursor`, `xkbcommon`, `fontconfig`, `freetype`, `harfbuzz`, `libudev`, `libxcb` | renderer GPUI (blade/Vulkan) di Linux |
| `libvulkan.so.1` + sebuah ICD | di mesin ini hanya Mesa `llvmpipe` (render perangkat lunak) yang terenumerasi — cukup untuk prototype, lihat [`docs/ui.md`](docs/ui.md) |
| `libxkbcommon-x11` (dev) | **tidak ada** di mesin ini; `scripts/env.sh` membuat symlink shim dan mengekspor `LIBRARY_PATH`. Perbaikan sistemnya: `sudo dnf install libxkbcommon-x11-devel` |

Selalu mulai shell dengan:

```bash
source scripts/env.sh
```

Skrip itu mengekspor `CARGO_HOME` (di dalam checkout: `.cargo-home`, sudah diisi
lewat reflink dari `~/.cargo` supaya tidak ada unduhan ulang dan sandbox tidak
perlu menulis ke home), `LIBCLANG_PATH=/usr/lib64` (ada `libclang.so.22.1`, tidak
ada symlink `libclang.so`), `LIBRARY_PATH` (shim `libxkbcommon-x11.so`),
`VK_ICD_FILENAMES` (llvmpipe) dan `XDG_RUNTIME_DIR` (socket Wayland).

`vendor/xattr` adalah salinan `xattr 0.2.3` dengan satu baris patch, dipasang
lewat `[patch.crates-io]` karena `gpui → gpui_http_client → zed-async-tar`
menariknya dan crate itu tidak lagi kompilabel terhadap `libc` modern. Alasannya
tercatat di [`vendor/xattr/VENDORED.md`](vendor/xattr/VENDORED.md).

## Mulai cepat

Semua perintah di bawah nyata: binary-nya ada dan flag-nya dibaca dari definisi
clap di `crates/cli/src/main.rs`, `crates/brain/src/main.rs`, `crates/ui/src/args.rs`
dan `crates/node/src/cli.rs`. `scripts/check.sh` tetap gerbangnya.

```bash
source scripts/env.sh                 # setiap shell baru

scripts/check.sh                      # fmt + clippy -D warnings + test (semua crate)
scripts/check.sh --fast               # sama, tanpa swarmbot-ui (build-nya menit-an)
```

**1. Demo offline** — empat bot, delegasi terskrip, tanpa kredensial dan tanpa
jaringan; brain-nya in-process:

```bash
cargo run -p swarmbot-cli -- demo                 # store SQLite di ./data
cargo run -p swarmbot-cli -- demo --offline       # store in-memory, tidak menulis apa pun
```

**2. Satu tugas ke tim** — kirim satu pesan, cetak semua yang tim lakukan, tunggu
idle, keluar (0 = selesai, 2 = melewati batas `--wait`):

```bash
cargo run -p swarmbot-cli -- run "periksa repo ini lalu tulis ringkasan" --backend fake
cargo run -p swarmbot-cli -- run "…" --team config/team.example.toml --group Team --wait 180
cargo run -p swarmbot-cli -- run "…" --json       # satu objek LiveEvent per baris
```

**3. Brain sebagai daemon, klien terpisah** — CLI dan UI berbicara lewat API yang
sama:

```bash
cargo run -p swarmbot-brain -- --data-dir ./data            # mencetak URL login sekali pakai
cargo run -p swarmbot-brain -- --data-dir ./data --offline --team config/team.example.toml
cargo run -p swarmbot-brain -- --print-config               # config terselesaikan, rahasia diredaksi
cargo run -p swarmbot-brain -- --data-dir ./data --check    # validasi + buka store, lalu keluar
cargo run -p swarmbot-cli -- --brain http://127.0.0.1:8787 --data-dir ./data bots ls
cargo run -p swarmbot-cli -- --brain http://127.0.0.1:8787 --data-dir ./data chat lead
cargo run -p swarmbot-cli -- --brain http://127.0.0.1:8787 --data-dir ./data nodes pair
cargo run -p swarmbot-cli -- --brain http://127.0.0.1:8787 --data-dir ./data shutdown
```

Flag brain yang berguna: `--addr HOST:PORT` (atau `--port`, yang mempertahankan
host), `--internal-addr ""` untuk mematikan listener MCP, `--public-origin` untuk
URL login dan allow-list `Origin`, `--team FILE`, `--backend ID`, `--root PATH`
(berulang), `--offline` (paksa backend `fake`: tidak ada panggilan model yang keluar
proses), `--no-ticker`, `--print-config`, `--check`. Setiap opsi panjang juga punya
variabel lingkungan (`SWARMBOT_DATA_DIR`, `SWARMBOT_PUBLIC_ADDR`, …), jadi brain di
bawah supervisor tidak butuh skrip pembungkus.

Hati-hati: **`--offline` berarti tiga hal berbeda di tiga binary** — `swarmbot-brain
--offline` memaksa backend `fake`; `swarmbot-ui --offline` men-spawn brain dengan
backend `fake` plus tim demo; `swarmbot demo --offline` memakai store in-memory
alih-alih SQLite.

Token dicari berurutan: `--token` → `SWARMBOT_TOKEN` → `<data-dir>/owner-token` →
ditanyakan di terminal (hanya kalau stdin benar-benar tty). `--config`, `--team`,
`--backend` dan `--root` **ditolak sebelum mendial** bila `--brain` dipakai: config
lokal menggambarkan brain yang akan dibangun proses ini, dan membacanya lalu diam-diam
tidak menerapkannya adalah kebohongan. `demo` embedded-only dan menolak `--brain`
dalam satu baris.

**4. Aplikasi desktop GPUI**:

```bash
cargo run -p swarmbot-ui -- --offline            # spawn brain (backend fake) + buka jendela
cargo run -p swarmbot-ui -- --selftest           # event fabrikasi → reducer → render → laporan → exit 0
cargo run -p swarmbot-ui -- --brain http://127.0.0.1:8787 --token "$TOK"
```

`--offline` menyiratkan `--spawn-brain`; UI menunggu `GET /api/health` jawab `ok`
(≤ 20 s) sebelum menganggap brain hidup, dan membaca `owner-token` yang brain itu
baru minting (4 percobaan × 120 ms) — jadi tidak ada endpoint yang dibebaskan dari
auth (D9.10). Detail: [`docs/ui.md`](docs/ui.md).

**5. Node di mesin lain** — pairing lewat kode sekali pakai, lalu daemon mendial ke
brain dan menjalankan tool di bawah policy-nya sendiri:

```bash
cargo run -p swarmbot-cli  -- --brain http://127.0.0.1:8787 --data-dir ./data nodes pair
cargo run -p swarmbot-node -- --data-dir ./node-data pair http://127.0.0.1:8787 ABCD-EFGH
cargo run -p swarmbot-node -- --data-dir ./node-data run --root ~/kerja --ceiling shell=ask
cargo run -p swarmbot-node -- --data-dir ./node-data status
```

`nodes pair` mencetak kode, masa kedaluwarsanya, baris `swarmbot-node pair …` yang
persis, dan catatan bahwa **minting bukan pairing**. Kodenya 8 karakter Crockford
base32, sekali pakai, 10 menit, dan brain hanya menyimpan hash-nya — jadi satu
penolakan yang sama untuk "tidak pernah dibuat", "kedaluwarsa" dan "sudah dipakai"
(supaya tidak jadi oracle untuk menebak kode). Kode dibakar **sebelum** apa pun
dibuat, tetapi pairing yang gagal di langkah awal tidak membakarnya, jadi node yang
salah ketik hostname bisa mencoba lagi dengan kode yang sama.

Semua milik node hidup di satu data dir: `identity.json` (kunci privat Ed25519,
0600, tidak pernah keluar dari node), `node.toml` (tanpa rahasia), `workspace/`
(satu-satunya root default), `audit/`, `spill/`, `node-state.json`. Ceiling node
selalu menang atas ceiling brain (`min(brain, node)` per permukaan), dan pencabutan
(`DELETE /api/nodes/{id}`) menutup socket dengan 4401 — node berhenti mendial dan
operator disuruh pair ulang.

**Konfigurasi**:

```bash
cp config/swarmbot.example.toml data/config.toml    # brain, backend, safety
swarmbot-brain --data-dir ./data --team config/team.example.toml   # 4 bot: lead/scout/dev/sentinel
```

Aturan yang dijamin loader (`crates/brain/src/config.rs`, `crates/cli/src/config.rs`):
precedence **CLI > environment > file config > default**; file config dibaca dari
`--config`, kalau tidak ada dari `<data-dir>/config.toml`; roster dari `--team FILE`
mengalahkan `[team]` inline; `<data-dir>/secrets.json` berisi `{"NAMA_ENV":
"kunci"}` dengan mode `0600` (loader memperingatkan kalau file itu bisa dibaca orang
lain); `<data-dir>/owner-token` adalah token login sekali pakai yang dicetak saat
boot pertama.

## Memakai model sungguhan

Empat backend, satu port (`swarmbot_llm::LlmBackend`). Backend `fake` **selalu**
ada walaupun tidak dikonfigurasi, jadi demo dan seluruh tes berjalan tanpa
kredensial dan tanpa jaringan.

**1. Gateway OpenAI-compatible** (`kind = "openai_compat"`) — SSE streaming dan
native tool call:

```toml
[[backends]]
id = "inferhub"
kind = "openai_compat"
base_url = "https://api.inferhub.dev/v1"
api_key_env = "INFERHUB_API_KEY"
model = "ali/qwen3.8-flash"
timeout_secs = 180
max_retries = 3

[backends.extra_body]
max_bid_per_mtok = 0.5        # ← parameter milik gateway, bukan milik kita
```

`extra_body` dan `extra_headers` digabung **apa adanya, paling akhir**, ke setiap
request. Itu sengaja: `api.inferhub.dev` melelang tiap request antar penyedia,
dan panggilan tanpa bid dijawab `402 no_provider_under_bid` beserta harga minimum
penyedia. Nama parameter bid bukan sesuatu yang boleh dikarang oleh kode ini,
jadi ia milik operator. Adapter mengenali 402 itu dan menambahkan petunjuk ke
pesan errornya (`crates/llm/src/openai.rs`, `refused_error`). 402 **tidak**
di-retry (hanya 429/5xx/timeout yang di-retry, dengan backoff + jitter dan
menghormati `Retry-After`).

**2. CLI harness** (`kind = "claude_cli"`) — memakai login `claude` milik
pemilik, tanpa API key:

```toml
[[backends]]
id = "claude"
kind = "claude_cli"
model = "sonnet"
[backends.options]
binary = "claude"
```

Hari ini adapter ini hanya `probe()` yang sungguh bekerja (mencari binary dan
menjalankan `--version`); `chat()` menjawab `Unavailable` dengan alasan yang
jujur. Loop tool milik harness sendiri (`owns_tool_loop() == true`) baru tersambung
lewat MCP gateway di M3.

**3. Agen ACP** (`kind = "acp"`) — satu adapter untuk setiap agen yang berbicara
[Agent Client Protocol](https://agentclientprotocol.com) (JSON-RPC 2.0 di atas
stdio): **opencode** (native: `opencode acp`), **Claude Code** (lewat adapter
`claude-code-acp` milik Zed) dan **Codex** (lewat adapter `codex-acp`). Yang
membedakan ketiganya hanya command-nya:

```toml
[[backends]]
id = "opencode"
kind = "acp"
[backends.options]
command = "opencode"
args = ["acp"]
# cwd = "/home/me/projects"     # default: direktori kerja brain
# permission_mode = "reject"    # default; atau "allow" (lihat di bawah)

[[backends]]
id = "claude"
kind = "acp"
# api_key_env = "ANTHROPIC_API_KEY"  # opsional; tanpa ini login `claude`
#                                    # milik pemilik yang dipakai
[backends.options]
command = "claude-code-acp"     # npm i -g @zed-industries/claude-code-acp
# command = "npx"               # alternatif tanpa install global:
# args = ["-y", "@zed-industries/claude-code-acp"]

[[backends]]
id = "codex"
kind = "acp"
[backends.options]
command = "codex-acp"           # npm i -g @zed-industries/codex-acp
```

Cara kerjanya dan batasnya, stated plainly:

- **Agen punya tangannya sendiri.** Ia menjalankan tool-nya sendiri (baca,
  tulis, shell) di bawah sandbox **miliknya** — policy engine dan node SwarmBot
  tidak menggerbangi aksi itu. Titik intervensi kita adalah
  `session/request_permission` milik ACP, dijawab sesuai `permission_mode`:
  `reject` (default) menolak setiap permintaan — agen tetap bisa membaca,
  berpikir dan menjawab, tapi tidak mengubah apa pun; `allow` memilih opsi
  *allow-once* agen untuk tiap aksi. `allow` adalah opt-in eksplisit operator,
  tidak pernah default, dan opsi `*_always` **tidak pernah** dipilih — tidak ada
  blanket grant yang hidup lebih lama dari satu turn. Setiap keputusan menulis
  satu baris log info (jejak audit minimum adapter ini).
- **Satu turn = satu sesi ACP baru.** Proses agen di-spawn sekali, dipertahankan
  antar turn (dan di-respawn kalau mati), tapi riwayat bot dirender utuh ke
  prompt tiap turn — persis seperti backend HTTP, tanpa konteks ganda. Karena
  itu `survives_restart`-nya jujur `false`: tidak ada sesi native untuk
  dipulihkan; memori bot ada di store kita.
- **`timeout_secs` menjadi timeout *idle*.** Selama agen mengirim update (chunk
  teks, pikiran, tool call) turn terus berjalan; keheningan sepanjang itu
  menggagalkan turn dan mengirim `session/cancel`.
- **`model` adalah keinginan, bukan jaminan.** Kalau agen mengiklankan state
  model ACP (masih *unstable*), kita pindah ke model itu dan penolakan menjadi
  error — bukan diam-diam memakai model lain. Kalau tidak, agen memakai model
  konfigurasinya sendiri dan `probe` + log mengatakannya. Untuk agen yang
  tidak mengiklankan daftar model sama sekali (opencode), operator bisa
  menunjuk `options.models_command` (mis. `["opencode", "models"]`): adapter
  membaca daftar dari CLI agen sendiri, dan kolom model di menu terisi.
- **Provider & model per bot dari UI.** Tombol `provider: …` di header
  percakapan DM membuka menu dua kolom: kiri daftar backend terkonfigurasi,
  kanan daftar model yang **backend itu sendiri iklankan** (`/models` untuk
  gateway OpenAI-compatible, state model sesi untuk agen ACP) plus kotak
  search; backend yang tidak mengiklankan daftar berkata begitu terus terang
  dan kotak search menjadi jalan teks bebas. *Apply* mengirim
  `PATCH /api/bots/{id}` dan berlaku mulai turn berikutnya; pilihannya tersimpan
  di store brain.
- **Kredit tidak diteruskan.** Anak proses memakai login agen milik pemilik
  (tidak ada token claude.ai/ChatGPT yang dibaca atau dikirim ke mana pun);
  kunci dari `api_key_env`/`secrets.json` disuntikkan hanya sebagai environment
  anak proses — tidak pernah argv, log, atau `Debug`. `inherit_env = false`
  menyaring environment anak seperti tool shell (D5.7).
- **`probe`** = spawn + `initialize` + `session/new`, tanpa panggilan model:
  layar Settings bisa bilang "agen siap / tidak ada / belum login" beserta
  metode auth yang agen iklankan.
- **Aktivitas tool agen belum ditranskrip** — satu baris log per aksi; transcript
  menampilkan teks dan reasoning. Tool delegasi kita (`message_bot`, dst.) juga
  belum menjangkau agen ACP; itu pekerjaan MCP gateway (M3), sama seperti untuk
  harness `claude_cli`. Shutdown koneksi menutup stdin agen lebih dulu (EOF —
  agen stdio yang baik keluar dan mengurus anaknya sendiri) lalu, setelah 2
  detik, membunuh seluruh grup prosesnya: wrapper `npx` tidak bisa
  melewatinya, dan `opencode acp` yang melepas child `serve`-nya ke grup
  sendiri (teramati di opencode 2.0.28) hanya bisa dibereskan lewat jalur EOF
  itu — SIGKILL tidak menjangkaunya. Batas yang jujur: `serve` yang sedang
  *hang* (mis. pemindaian watcher-nya macet di mesin yang sibuk) bisa tetap
  hidup; pemilik melihatnya sebagai `opencode serve --stdio` dan
  menghabisinya sendiri.

**4. `fake`** — skrip deterministik untuk tes dan demo offline; merekam request
yang ia terima supaya orkestrasi bisa diassert.

Kunci hanya berasal dari dua tempat: variabel lingkungan yang dinamai
`api_key_env`, atau `<data>/secrets.json` (mode `0600`, bentuk
`{"INFERHUB_API_KEY": "sk-…"}`; secrets store menang atas environment). Kunci **tidak pernah** masuk repo, tidak pernah masuk SQLite, tidak
pernah ditaruh di argv, dan diredaksi dari `Debug` maupun log — tipe yang bisa
memegangnya mengimplementasikan `Debug` manual dan ada tes yang memastikannya.
Login `claude.ai`/ChatGPT milik bot tidak pernah dibaca atau diteruskan.

## Crate

| Crate | Isi | Status (tes dari run terakhir) |
| --- | --- | --- |
| `swarmbot-contracts` | Domain types shared by every SwarmBot crate: ids, transcript entries, deliveries, turns, tasks, requests, nodes, tools, errors and live events. | selesai — 69 |
| `swarmbot-llm` | LLM backend port and adapters: OpenAI-compatible HTTP (SSE + tool calls), ACP agent harnesses (opencode / Claude Code / Codex), CLI harnesses, and a deterministic Fake for tests. | selesai — 96 (+2 `#[ignore]`: gateway live `PROTO_LIVE_LLM`, agen ACP live `PROTO_LIVE_ACP`). `AcpAgent`: nyata end-to-end — 15 tes unit + 15 tes integrasi offline (agen terskrip in-memory + `/bin/sh` lewat stdio nyata) dan satu turn live terverifikasi lewat `codex-acp`. `ClaudeCli`: `probe()` nyata, `chat()` menjawab `Unavailable` dengan alasan jujur |
| `swarmbot-tools` | Tool registry and local handlers (shell, fs, glob, grep) with the path policy, safety ceilings, output spilling and audit log. | selesai — 95 |
| `swarmbot-store` | SQLite projections (WAL, migrations), the append-only JSONL transcript mirror and boot recovery. | selesai — 11 (5 lib + 2 memory + 4 sqlite; satu contract suite dijalankan ke dua store) |
| `swarmbot-core` | Orchestration: one runner per bot, deliveries, group routing, bot-to-bot messaging, handoffs, background tasks, approvals, instructions and the live hub. | selesai — 168 (159 lib + 9 integrasi delegasi), termasuk `bot_tools.rs`: 13 tool delegasi |
| `swarmbot-client` | Blocking brain API client (REST + live WebSocket) shared by the CLI and the GPUI desktop app. | selesai — 38 (37 + 1 doctest): `BrainClient`, `LiveSubscription`, `ClientError`, `StubBrain` di balik `features = ["test-support"]` |
| `swarmbot-node-protocol` | Node wire protocol: bidirectional JSON-RPC mux, Ed25519 pairing and signed upgrades, one-time stream tokens, typed errors. | selesai — 70 (67 lib + 2 WebSocket + 1 doctest): `wire`, `mux`, `auth`, `pairing`, `identity`, `streams`, `rpc`, `testing` |
| `swarmbot-brain` | Composition root: the brain daemon with the public API, the live WebSocket, the node gateway and the in-process local node. | selesai — 169 (121 lib + 14 api + 10 live + 8 node-gateway + 8 operations + 6 cli + 2 client-contract) |
| `swarmbot-cli` (bin `swarmbot`) | Terminal client: chat with bots, run one-shot tasks, watch delegation, answer approvals, pair nodes. | selesai — 106. Dua mode lewat satu port `Session`: `Embedded` (in-process) dan `Remote` (`--brain`) |
| `swarmbot-node` (bin `swarmbot-node`) | The hands: a node daemon that pairs with a brain and executes fs/shell RPCs under its own policy. | selesai — 135 (71 lib + 64 integrasi, 0 diabaikan), termasuk `tests/real_brain.rs` (3) yang menjalankan binary node sungguhan melawan brain nyata |
| `swarmbot-ui` (bin `swarmbot-ui`) | GPUI desktop app: sidebar, conversations, live streaming transcript, delegation graph, approval cards, nodes and settings. | selesai — 39. 8.797 baris: `view/{sidebar,conversation,composer,graph,cards,nodes,settings,palette}`, `widgets/{text_input,markdown,bits}`, `reducer`, `worker`, `spawn` |
| `swarmbot-mcp` | Exposes the SwarmBot tool registry as an MCP server (stdio for CLI harnesses, streamable HTTP for the internal listener). | **scaffold** (M3) — satu-satunya crate yang belum ditulis |

Keterangan crate adalah string `description` dari `Cargo.toml` masing-masing; angka
tes dari `cargo test --workspace` (bukan hasil grep).

## Status milestone

| Milestone | Cakupan | Status |
| --- | --- | --- |
| **M0** Fondasi | `contracts` (14 modul + `lib.rs`), `store` (`SqliteStore` + `MemoryStore` + `JsonlMirror` + satu contract suite untuk keduanya) | **selesai** |
| **M1** Agent loop + tool lokal | `llm` (`OpenAiCompat` SSE + tool call + retry + hint 402, `FakeBackend`, `BackendRegistry`, `ClaudeCli` probe), `tools` (registry, `PathPolicy`, `PolicyEngine`, `AuditLog`, `LocalExecutor`, 7 handler), `core/runners` (loop turn, streaming ke transcript, tool rounds, kompaksi) | **selesai** |
| **M2** Delegasi multi-agen | `core`: `router`, `messaging`, `tasks` (`submit` satu pintu untuk `start_task` dan `handoff_task`), `approval`, `instructions`, `bus`, `transcript`, `session`, `swarm`, `compose`, `bot_tools` (13 tool) | **selesai** |
| **M3** MCP gateway + backend Claude | `crates/mcp`, adapter stream-json `claude -p` | **belum** — satu-satunya milestone yang belum dimulai |
| **M4** Brain API + CLI | `crates/brain` (axum REST + `/ws/live` + auth owner), `crates/client`, `crates/cli` (embedded + remote) | **selesai** |
| **M5** UI desktop GPUI | jendela nyata, sidebar, transcript streaming, composer, delegation graph, kartu approval, nodes, settings, palet | **selesai** (render lewat llvmpipe di mesin ini — lihat [`docs/ui.md`](docs/ui.md) §2) |
| **M6** Node daemon | `crates/node-protocol` (mux, pairing, upgrade bertanda tangan), `crates/node` (daemon: pair/run/status, policy sendiri, audit, redial), `brain/src/gateway/` (roster, dispatcher, socket axum) | **selesai**: pairing → hello → `fs.*` → `shell.exec` streaming → revoke 4401 teruji tiga lapis (wire 70 tes, daemon 135, gateway 8 di `brain/tests/node_gateway.rs`), dan `crates/node/tests/real_brain.rs` (3 tes, ~0,4 s, selalu jalan) menjalankan **dua sisi nyata sekaligus**: brain in-process + binary `swarmbot-node` sebagai child, termasuk tool call jarak jauh yang dibuktikan lewat berkas penanda |
| **M7** Penguatan | routines, memory, skills, data stream `/node/stream/:id`, backend `codex app-server`, backup | **belum** |

Run terakhir yang tercatat: `scripts/check.sh` exit 0 — fmt + clippy + tes,
**1021 lulus, 0 gagal, 2 di-`#[ignore]`**. Yang diabaikan memang butuh sesuatu
yang tidak ada di gate: tes gateway live llm (`PROTO_LIVE_LLM=1`) dan tes agen
ACP live (`PROTO_LIVE_ACP=1`, plus `PROTO_LIVE_ACP_CMD`; tanpa
`PROTO_LIVE_ACP_PROMPT=1` ia berhenti di handshake yang gratis).
Perincian per crate ada di tabel di atas; `scripts/check.sh` adalah yang menentukan.

Catatan yang belum ada di kode mana pun: repository ini **belum** `git init`.

### Yang belum diverifikasi

Ditulis di sini supaya tidak ada yang mengira ini sudah terbukti:

| Jalur | Kenapa belum |
| --- | --- |
| TLS: `wss://` dan origin `https://` | rewrite URL-nya ada dan teruji sebagai string, rustls ter-link, tapi tidak ada brain TLS di sini untuk didial |
| Prompt token di terminal | butuh tty sungguhan; tes memakai cabang `stdin_is_tty = false`, yaitu cabang yang **tidak boleh** bertanya |
| Tool call jarak jauh dari sebuah turn bot di **dua proses OS terpisah, dengan model sungguhan** | `real_brain.rs` menyusun brain secara in-process supaya backend `Fake` yang terskrip bisa dipakai, sedangkan run dua-proses (`target/two-process/run.sh`) tidak bisa disodori turn terskrip. Jadi kedua sisi sudah terbukti dan gabungannya terbukti dengan satu proses yang dibagi; pasangan yang benar-benar terpisah dengan model live belum pernah dijalankan |
| SIGINT dan jalur keluar paksa sinyal kedua (`registry.kill_all()` lalu `exit(0)`) | pengiriman sinyal ke child yang disupervisi tidak bisa dipalsukan dengan jujur di unit test |
| Alasan penolakan saat **redial** node yang dicabut | brain menjawab 401 sebelum upgrade dan tungstenite hanya melaporkan `HTTP error: 401 Unauthorized`; body penolakan ("node … is revoked") tidak dibaca, jadi log operator bilang "unreachable". Kasus socket yang masih hidup sudah benar (4401 → `StopReason::Revoked`) |
| `LiveEvent::Resync` karena broadcast yang **lagged** | resync akibat cursor basi/epoch lain teruji (`brain/tests/live.rs`, `client_contract.rs`); cabang `Lagged` di `LiveHub::subscribe` tidak punya pemicu deterministik |
| Frame rate GPUI di GPU nyata | sesi ini tidak punya `/dev/dri`; semua render lewat llvmpipe |
| Windows | `crates/node` memanggil `libc::gethostname` dan `killpg` tanpa syarat, jadi daemon-nya unix-only dalam praktik |
| Agen ACP **nyata** di dalam test suite | suite memakai agen terskrip in-memory (seluruh protokol: streaming, permission, cancel, timeout, respawn) plus `/bin/sh` lewat stdio nyata (framing + kill grup proses). Agen sungguhan diverifikasi manual di mesin ini: handshake + `session/new` untuk **opencode**, **claude-code-acp** dan **codex-acp**, dan **satu turn penuh lewat codex-acp** (balasan `PONG`, 5,4 s — satu panggilan model kecil). Turn penuh opencode/Claude Code belum: opencode konfigurasi global pemilik menggantung di `session/new` (sisi opencode, bukan adapter — timeout setup kita menangkapnya dengan jujur), dan Claude Agent SDK tidak menemukan kredensial di mesin ini. Tes live satu-turn ada di gate env: `PROTO_LIVE_ACP=1` |

## Keamanan

Penjelasan lengkap dan alasannya: [`docs/decisions.md`](docs/decisions.md) D1.4,
D5.1–D5.7. Ringkasnya:

- **Dua lapis policy.** Brain memutuskan lebih dulu (aturan, kartu persetujuan);
  node memutuskan terakhir. Ceiling efektif = `min(brain, node)` per permukaan
  (`shell`, `read_file`, `write_file`, `edit_file`), jadi brain yang keliru atau
  dibajak tidak pernah bisa menaikkan apa yang mau dilakukan sebuah node.
- **Batas filesystem.** `PathPolicy`: roots dengan containment realpath per
  komponen (symlink tidak bisa keluar root), root read-only, dan deny list bawaan
  (`~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.config/gh`, `~/.codex/auth.json`,
  `~/.claude*`, `~/.kenari_key`, `~/.env`, `/etc/shadow`, `/etc/sudoers`, serta
  identity, `secrets.json` dan direktori `nodes` SwarmBot sendiri di
  `~/.local/share/swarmbot`).
- **Perintah destruktif selalu bertanya**, bahkan dengan ceiling `shell = always`:
  `rm -rf`/`rm -fr`, `sudo`, `su`, `mkfs`, `dd if=`, `:(){`, `chmod -R 777`,
  `git reset --hard`, `git clean -fd`, `curl … | sh`, `wget … | sh`, `> /dev/sd`,
  `kill -9 1`, `shutdown`, `reboot`, `truncate -s 0 /` (daftar lengkap:
  `DESTRUCTIVE_PATTERNS` di `crates/tools/src/policy.rs`). Matcher-nya mengosongkan teks dalam tanda kutip (jadi
  `echo "rm -rf"` tidak ditandai) dan meng-inline payload `sh -c '…'`; ia **bukan**
  parser shell, dan dokumen kodenya mengatakan itu. Karena itu sebuah kecocokan
  hanya menaikkan keputusan menjadi `Ask`, tidak pernah `Refuse`.
- **Process group, selalu.** `shell` berjalan dengan `process_group(0)` dan
  seluruh grup dibunuh (`killpg(SIGKILL)`) saat timeout (default 120 s, maksimum
  900 s), saat owner menghentikan turn, atau saat socket putus. Tidak ada anak
  proses yang tertinggal tanpa pemilik.
- **Environment disaring.** Anak proses hanya menerima `PATH`, `HOME`, `LANG`,
  `LC_ALL`, `TERM`, `TMPDIR` plus `env` yang diminta eksplisit — tidak pernah
  environment brain yang mungkin memegang API key.
- **Agen ACP: sandbox agen, permission agen.** Backend `kind = "acp"`
  men-spawn agen asing (opencode/claude-code-acp/codex-acp) yang menjalankan
  tool-nya sendiri di bawah sandbox-nya sendiri — **bukan** di bawah policy
  SwarmBot, dan README bagian backend mengatakan itu terus terang. Gerbang kita:
  `permission_mode` (default `reject`) yang hanya pernah memilih opsi
  *sekali* — tidak pernah `*_always`. Shutdown menutup stdin agen (EOF) dan,
  bila agen tidak keluar dalam 2 detik, seluruh grup prosesnya di-`killpg`,
  jadi wrapper `npx` tidak bisa bertahan sebagai yatim; child yang agen
  lepas ke grupnya sendiri (opencode `serve`) diurus oleh jalur EOF. Anak
  proses mewarisi environment pemilik (itu cara agen memakai login pemilik);
  `inherit_env = false` menyaringnya seperti aturan di atas.
- **Audit.** Setiap invocation menulis satu baris JSONL
  (`<data>/audit/<node>.jsonl`): waktu, invocation/bot/turn/task, node, tool,
  surface, target (command line atau path — bukan isi file), keputusan
  (`run`/`ask`/`allow`/`deny`/`refuse`), hasil, durasi, ukuran. Audit yang gagal menulis
  tidak pernah menggagalkan tool call.
- **Node menyimpan semuanya di satu data dir**: `identity.json` (kunci privat
  Ed25519, 0600, tidak pernah keluar dari node), `node.toml` (tanpa rahasia),
  `workspace/` (satu-satunya root default), `audit/`, `spill/`, `node-state.json`.
  Catatan jujur: **`spill/` tidak berada di dalam root secara default**, jadi
  keluaran yang di-spill tidak bisa dibaca balik lewat `fs.read` kecuali pemilik
  menambah root yang meliputinya (`--root <data>/spill`). Tanpa itu, teks spill yang
  diterima model menunjuk berkas yang tidak bisa ia buka.
- **Tidak ada endpoint tanpa auth.** Termasuk `/api/health`: UI yang men-spawn
  brain membaca `owner-token` yang brain itu baru minting (D9.10) alih-alih meminta
  pengecualian. Upgrade WebSocket juga wajib lolos allow-list `Origin`, karena
  browser akan menempelkan cookie sesi ke halaman dari mana pun.
- **Satu pemilik, loopback saja.** Listener publik default `127.0.0.1:8787`;
  akses dari luar lewat reverse proxy atau tailnet, bukan `0.0.0.0`.
- **Kartu persetujuan kedaluwarsa** setelah `approval_expiry_secs` (600 s), supaya
  kartu yang tak dijawab tidak menahan turn selamanya. `ask_user` dan
  `request_approval` tidak pernah memblokir tool call: keduanya langsung
  mengembalikan `{status: pending, id}` dan jawabannya datang sebagai delivery.

## Dokumentasi

| Berkas | Isi |
| --- | --- |
| [`docs/decisions.md`](docs/decisions.md) | Catatan keputusan yang mengikat (D0–D8), termasuk penyimpangan sengaja dari OpenBot |
| [`docs/architecture.md`](docs/architecture.md) | Graf dependensi crate, model domain, siklus hidup sebuah pesan dan sebuah handoff, live hub, penyimpanan, ringkasan protokol node |
| [`docs/tools.md`](docs/tools.md) | Setiap tool (7 node + 13 bot): argumen sesuai `spec()` aslinya, keluaran, surface dan ceiling yang mengaturnya, aturan hasil |
| [`docs/ui.md`](docs/ui.md) | Aplikasi desktop GPUI: batas proses/executor, layar, fakta lingkungan yang terverifikasi |
| [`docs/node-protocol.md`](docs/node-protocol.md) | Subset protokol node, ditulis agar sebuah node bisa diimplementasikan dalam bahasa lain |
| [`AGENTS.md`](AGENTS.md) | Aturan kerja untuk agen (manusia atau AI) yang melanjutkan repo ini |

Referensi luar: desain ini meminjam bentuknya dari OpenBot
(`../dbot/docs/decisions.md`, `../dbot/docs/node-protocol.md`). Tidak ada
kode, font, logo, atau aset xAI/Grok di repo ini; avatar dihasilkan dari hash id
bot.

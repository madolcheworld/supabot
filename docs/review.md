# Tinjauan kode — November 2026

Analisa menyeluruh 12 crate (109.460 baris Rust), dilakukan dengan membaca kode,
menjalankan gate, dan memverifikasi setiap temuan berat di sumbernya. Dokumen ini
menjawab dua hal: **apa yang sudah benar** dan **apa yang harus dikerjakan
berikutnya**.

Basis: `scripts/check.sh` (full, termasuk `swarmbot-ui`) **exit 0** — fmt + clippy
`-D warnings` + **1240 lulus, 0 gagal, 2 di-`#[ignore]`**.

> **Status pelaksanaan: seluruh tahap di §5 sudah dikerjakan.** Lihat §8 untuk apa
> yang berubah, apa yang tidak, dan apa yang tetap tidak terverifikasi. Dokumen ini
> disimpan sebagai catatan *mengapa* perubahan itu ada; §2–§4 tetap menggambarkan
> kondisi kode sebelum pekerjaan itu, dan setiap temuannya menandai tes yang kini
> menguncinya.
>
> **Putaran kedua ada di §9**: flake `real_brain` yang di §8 masih terbuka sekarang
> terdiagnosis (bukan soal beban), dua debt terbesar dibayar, dan dua permukaan baru
> ditambahkan (usage, backup). Angka gate yang berlaku ada di
> [`README.md`](../README.md#status-milestone), bukan di header ini.

---

## 1. Ringkasan

Yang membuat repo ini layak dilanjutkan, bukan sekadar prototipe:

* **Batas antar-crate benar-benar ditegakkan.** `core` tidak pernah membuka socket,
  `tools` tidak pernah mengimpor `core`, `ui` hanya lewat `client`. Diverifikasi
  dari 12 `Cargo.toml` — tidak ada satu pun edge yang melanggar aturan di
  `AGENTS.md`.
* **Kedisiplinan rahasia nyata, bukan slogan.** `Debug` ditulis manual pada setiap
  tipe pembawa kunci (`llm/src/openai.rs:295`, `llm/src/acp/mod.rs:307`,
  `cli/src/remote.rs:146`), kunci hanya di header/env anak, dan ada tes yang
  memastikan byte-nya tidak muncul di log/error.
* **`Ok(ToolResult::error("denied…"))` vs `Err` dipisahkan dengan sadar.** Penolakan
  adalah data, kegagalan adalah error — dan itu konsisten di jalur tool.
* **Store punya contract suite bersama** untuk `SqliteStore` + `MemoryStore` +
  SQLite in-memory, dengan konflik revisi di dalam satu transaksi
  (`store/src/sqlite.rs:684`). Ini kelas pengujian yang jarang ada.

Yang harus diperbaiki: **6 bug P0** (dua di antaranya lubang keamanan nyata), satu
fitur yang 90% selesai tapi tidak pernah menyala, dan dokumentasi status yang
menyesatkan arah pengembangan.

---

## 2. P0 — perbaiki sebelum menambah fitur

### 2.1 Satu "always allow" membatalkan lantai destruktif (keamanan)

`PolicyEngine::decide` menaikkan `rm -rf /` ke `Ask` *walaupun* ceiling `Always`
(`tools/src/policy.rs:526-536`). Tapi `Ask` lalu masuk ke gate
(`tools/src/handlers/mod.rs:184-215`), dan gate memeriksa aturan tersimpan:

* `rule_from` menyimpan `command_prefix` = kata pertama perintah
  (`core/src/approval.rs:583-604`),
* pencocokan hanya `target.starts_with(prefix)`
  (`contracts/src/request.rs:341-346`).

Jadi satu "always allow" pada `rm build/x` menyimpan prefix `rm`, dan setelah itu
`rm -rf /` **lolos otomatis**. Hal yang sama membuat `ls` cocok dengan `lsof`
(tidak ada batas kata). Ini melanggar D5.5 yang menyebut lantai destruktif sebagai
absolut.

**Perbaikan:** (a) tolak aturan `Allow` bila `is_destructive(command)`; (b) cocokkan
prefix pada batas kata (`split_whitespace().next()` di kedua sisi, bukan
`starts_with`); (c) simpan `surface + tool + prefix` dan jangan pernah mengizinkan
prefix tunggal untuk shell.

### 2.2 `auto_approve_read_only` membatalkan aturan `deny`

`ApprovalService::ask` memeriksa `auto_approve_read_only` **sebelum**
`evaluate()` (`core/src/approval.rs:144-150`). Default-nya `true`
(`contracts/src/config.rs:253`). Akibatnya aturan `deny` untuk `read_file` —
satu-satunya cara pemilik melarang bot membaca sesuatu — tidak pernah dievaluasi.
Urutan yang benar adalah "aturan dulu, baru kenyamanan"; ini kebalikannya.

**Perbaikan:** pindahkan blok `auto_approve_read_only` ke setelah `evaluate()`,
dan hanya untuk kasus `Ask`.

### 2.3 `shell` berada di luar `PathPolicy`

Hanya `cwd` yang di-resolve (`tools/src/handlers/shell.rs:163-174`); argumen
perintah tidak disentuh. `sh -c 'cat ~/.ssh/id_rsa'` berjalan di dalam root tanpa
melewati deny list, sementara `docs/tools.md:88-92` menjanjikan batas itu mencakup
"setiap path yang disentuh handler". Penyaringan env hanya menutup `env`, bukan
membaca berkas.

**Perbaikan:** paling tidak, tolak perintah yang menyebut path di deny list atau di
luar root (heuristic, dan katakan begitu di doc — seperti pola matcher destruktif);
lebih baik: jalankan shell di bawah resolusi yang tidak bisa keluar root. Bila
keduanya belum, **nyatakan batasnya di `docs/tools.md`** alih-alih membiarkan
dokumen menjanjikan lebih.

### 2.4 Bot yang di-disable lalu di-enable menjadi tuli permanen

`BotService::set_enabled(false)` memanggil `handle.shutdown()` tetapi **tidak**
`runners.unregister(id)` (`core/src/bots.rs:172-174`). `ensure_runners` melewati
bot yang masih punya handle (`core/src/swarm.rs:364-366`), sehingga handle mati itu
bertahan. Setelah di-enable kembali, setiap `deliver` gagal dengan
`"the runner for … is gone"` (`core/src/runners.rs:126-130`) dan pesan pemilik
tertinggal `Queued` selamanya.

**Perbaikan:** `unregister` saat disable (satu baris), atau buat `ensure_runners`
mengganti handle yang channel-nya sudah tertutup.

### 2.5 `Swarm::shutdown()` bisa menggantung

`run_turn` menandai `busy = true` (`core/src/runners.rs:694`) dan membersihkannya
di baris terakhirnya (`:795`). Jalur cancel **menjatuhkan future turn**
(`:462-465`), jadi `:795` tidak pernah jalan dan `busy` tetap `true`.
`RunnerHandle::joined()` lalu menunggu `finished.notified()`
(`:173-178`) — dan `notify_waiters()` tidak menyimpan permit, jadi bila task sudah
selesai sebelum `joined()` dipanggil, penantian itu tidak pernah bangun. Hasil:
brain tidak bisa berhenti dengan bersih saat ada turn yang di-cancel.

**Perbaikan:** clear `busy` lewat drop guard di dalam `run_turn`, dan ganti
handshake `Notify` dengan `JoinHandle`/`watch` yang menyimpan status terminal.

### 2.6 Stop tidak membatalkan kartu approval

`ApprovalService::cancel_turn` ada (`core/src/approval.rs:315`) tetapi **tidak
punya pemanggil di kode produksi** (hanya tes). Owner menekan Stop →
`orchestrator::interrupt` hanya memicu token turn; `ask()` tetap menunggu sampai
`approval_expiry_secs` (default 600 s). Turn yang "dihentikan" tetap hidup
sepuluh menit.

**Perbaikan:** panggil `cancel_turn` dari `Orchestrator::interrupt`, dan `select!`
pada `ctx.cancel` di dalam `ask()`.

---

## 3. P1 — bug fungsional yang terlihat pemakai

| # | Temuan | Bukti |
| --- | --- | --- |
| 3.1 | **Routines tidak pernah menyala.** Endpoint, client, UI, dan scheduler semuanya ada, tetapi `start_routine_ticker()` tidak dipanggil di mana pun (hanya didefinisikan, `core/src/swarm.rs:416`). | `grep start_routine_ticker` → hanya definisi |
| 3.2 | **Status run routine selamanya `Running`.** `RoutineRun::complete/fail` tidak pernah dipanggil; `update_run` tanpa pemanggil produksi. Tabel tumbuh satu baris per tick dengan kebohongan status. | `core/src/routines.rs:123-135` |
| 3.3 | **`/mcp` di listener internal tanpa autentikasi** dan melayani seluruh registry (termasuk `shell`). Default-nya loopback (`contracts/src/config.rs:350`), tetapi tidak ada validasi bila operator menulis `0.0.0.0`. | `brain/src/routes/mod.rs:136-147` |
| 3.4 | **UI tidak pernah menampilkan error.** Satu-satunya perender `state.toast` dan `ConnectionState` adalah `render_top_bar` yang `#[allow(dead_code)]` tanpa pemanggil (`ui/src/view/mod.rs:1813`). Kirim gagal, simpan gagal, socket mati — semuanya tidak terlihat. | diverifikasi: hanya definisi |
| 3.5 | **Exit code 2 berarti dua hal** — "timeout" (`cli/src/main.rs:9-11`) dan error pemakaian clap (`:233`). Skrip pembungkus tidak bisa membedakan flag salah dari tim yang lambat. Stream yang berakhir normal juga dilaporkan "TIMED OUT". | `cli/src/main.rs:731-786` |
| 3.6 | **`events_log` tidak pernah dipangkas.** `LiveHub::truncate_log` ada, tetapi nol pemanggil (`core/src/bus.rs:333`); doc-nya justru mengklaim log dibatasi. Setiap perubahan = satu baris permanen. | diverifikasi |
| 3.7 | **`JsonlMirror::rebuild` hanya memulihkan entry**, bukan baris conversation/`last_seq` (`store/src/mirror.rs:107-121`) — store hasil rebuild tidak bisa di-append (`core/src/transcript.rs:62-82`). "Escape hatch" saat DB hilang hanya separuh bekerja. | diverifikasi |
| 3.8 | **Catch-up grup melewatkan pesan tanpa jejak.** Batas 60 entry (`runners.rs:63`) tetapi hanya batas 8 KiB yang menandai `truncated`; dengan >60 baris pendek, entry delivery lalu ditandai `seen` dan pesan di antaranya hilang permanen (`:1389-1401`). | diverifikasi |
| 3.9 | **Node tanpa deadline per-call.** `NodeDispatcher` tidak memberi timeout (`brain/src/gateway/executor.rs:174-181`); node yang hidup tapi menggantung di handler membekukan turn bot itu selamanya. | `node-protocol/src/mux.rs:354-372` |
| 3.10 | **Audit JSONL tidak atomik.** Dua `write_all` terpisah (`tools/src/audit.rs:232-233`) padahal doc-nya menjanjikan satu tulis atomik; dua turn bersamaan bisa merusak satu baris. | diverifikasi |
| 3.11 | **Spill & audit world-readable** (`tools/src/registry.rs:364-373`, mode default 0644/0755), padahal isinya output perintah dan isi berkas. `identity.json` sudah benar 0600 — tetangganya tidak. | diverifikasi |
| 3.12 | **`write_file` merusak mode berkas**: `File::create` (0644) + rename mengganti berkas 0600. | `tools/src/handlers/write_file.rs:166-186` |

---

## 4. P2 — akumulasi yang perlu dijaga

* **Store sinkron di dalam async.** `Mutex<Connection>` + `busy_timeout=5000`
  (`store/src/sqlite.rs:260`), dipanggil inline dari task tokio. Satu penulis
  terkontensi memarkir worker sampai 5 detik dan menyerialkan seluruh akses DB.
  Ini utang arsitektural terbesar yang tersisa.
* **Pertumbuhan tanpa batas**: `tool_calls` (satu baris per panggilan, teks hasil
  penuh, tanpa prune), `routine_runs`, `cards` di UI, dan `entries` di UI yang
  di-*re-sort* seluruhnya per entry baru (`ui/src/state.rs:707`) di main thread.
* **Stream terpotong dianggap jawaban lengkap** (`llm/src/openai.rs:639-642`): EOF
  tanpa `[DONE]` dan tanpa `finish_reason` tetap `Ok`, lalu ditulis ke transkrip.
  Tidak ada `LlmEvent::Restart` sehingga retry menggandakan teks yang sudah tampil.
* **Satu setup ACP timeout mematikan semua turn bersamaan** karena
  `conn.shutdown()` dipanggil pada koneksi yang dipakai bersama
  (`llm/src/acp/mod.rs:717-720`).
* **Sesi approval `respond` vs `expire_one` tanpa CAS** (`core/src/approval.rs:239-257`
  vs `:292-303`): Allow pemilik bisa ditimpa Expired.
* **`ClaudeCli` mengklaim `steer: true` tetapi menutup stdin** setelah prompt
  (`llm/src/harness.rs:210-213` vs `:369`).
* **Delivery `Running` setelah crash tidak pernah di-reset** (`contracts/src/repo.rs:373-376`);
  `drop_queued` hanya menghapus dari memori sehingga `wait_for_idle` bisa menunggu
  selamanya (`runners.rs:582-588`).
* **`compaction` bisa memotong di tengah blok tool** (`core/src/session.rs:136-152`),
  menghasilkan sesi yang ditolak backend [OI]-compatible sampai restart.

---

## 5. Roadmap pengembangan

Urutan ini dipilih supaya setiap langkah membuat langkah berikutnya lebih aman,
bukan sekadar "yang paling menarik".

### Tahap 0 — Kejujuran & keamanan (1–2 hari, S/M)
1. P0 2.1, 2.2 (aturan approval) + tes: "always allow" vs `rm -rf /`, dan `deny`
   rule vs `auto_approve_read_only`.
2. P0 2.4, 2.5, 2.6 (runner liveness, shutdown, cancel approval) + tes
   shutdown-saat-mid-turn.
3. P0 2.3: minimal tolak path terlarang di `shell`, dan **perbaiki `docs/tools.md`**
   bila batasnya tetap heuristic.
4. P1 3.3: autentikasi `/mcp` (token owner atau token per-turn seperti
   `/mcp/t/{token}`) + validasi `internal_addr` harus loopback saat resolve.
5. P1 3.10, 3.11, 3.12: satu `write_all` per baris audit, mode 0600 untuk
   spill/audit, pertahankan mode berkas di `write_file`.

### Tahap 1 — Nyalakan yang sudah 90% jadi (1–2 hari, S)
6. P1 3.1 + 3.2: panggil `start_routine_ticker()` di brain & CLI embedded, lalu
   sambungkan `RoutineRun::complete/fail` ke hasil turn. Routines langsung menjadi
   fitur nyata, bukan tabel.
7. P1 3.6: panggil `truncate_log` pada timer; tambah `prune_*` di port untuk
   `tool_calls` dan delivery/turn yang sudah selesai.
8. P1 3.4 + 3.5: render top bar (toast + status koneksi) dan pisahkan exit code
   timeout dari error pemakaian.

### Tahap 2 — Sisa M3 yang sebenarnya sudah jalan (M)
9. `/mcp` per-turn untuk `ClaudeCli` (sekarang `--mcp-config` menunjuk ke `/mcp`
   statis tanpa filter, `llm/src/harness.rs:259-270`), lalu tutup
   `steer: true` yang bohong.
10. `serve_stdio` belum punya pemanggil di luar tes — sambungkan atau hapus dari
    doc crate.

### Tahap 3 — Penguatan (M/L)
11. Store keluar dari reactor: satu writer task + `spawn_blocking` gateway untuk
    port `Persistence`. Ini yang membuka jalan ke konkurensi yang lebih tinggi.
12. Deadline per-call di `NodeDispatcher` + `interrupt` saat habis (P1 3.9).
13. Selesaikan disaster recovery: `JsonlMirror::rebuild` memulihkan conversation
    dan `last_seq`, plus `VACUUM INTO` untuk backup.
14. `LlmEvent::Restart` + deteksi truncation (P2), isolasi kegagalan ACP per sesi.

### Tahap 4 — M7 yang tersisa
15. **Memory**: `MEMORY.md` sudah dibaca (`runners.rs:855`) dan di-cap 4 KiB
    (`instructions.rs:289`), tetapi **tidak ada yang menulisnya** selain seed
    kosong (`core/src/compose.rs:142`). Yang dibutuhkan: satu tool `remember`
    (append + dedup + cap) dan kebijakan kapan bot boleh memakainya.
16. **Skills**: `Bot.skills` sudah tersimpan dan dirender ke prompt
    (`instructions.rs:62`), tetapi belum ada pustaka skill. Langkah termurah:
    direktori `<data>/skills/<name>/SKILL.md` yang di-resolve saat render —
    tanpa tabel baru.
17. **Data stream** `/node/stream/:id` (`streams.rs` sudah mencetak token, framing
    belum ada) dan backend `codex app-server`.

---

## 6. Dokumentasi status menyesatkan — perbaiki sekarang

Ini bukan kosmetik: roadmap di README akan mengarahkan pekerjaan ke tempat yang
salah.

| Klaim di dokumen | Kenyataan di kode |
| --- | --- |
| `README.md:373` — M3 "belum", `crates/mcp` scaffold | `crates/mcp` 543 baris + 10 tes; `/mcp` dan `/mcp/t/{token}` ter-mount di brain |
| `README.md:377` — M7 "belum" | routines (routes + client + UI), memory read-path, dan `Bot.skills` sudah ada — yang belum hanya ticker, penulisan memory, dan pustaka skill |
| `AGENTS.md:214` — `ClaudeCli::chat` mengembalikan `Unavailable` | `chat()` diimplementasikan penuh (`harness.rs:217-578`) |
| `AGENTS.md:41`, `README.md:386` — "belum `git init`" | repo git dengan 13 commit |
| `docs/architecture.md:19` — "965 passed, 1 ignored" | 1240 passed, 2 ignored |
| `docs/architecture.md:94` — routines "no table yet" | `routine_runs` + `MIGRATION_0002_ROUTINES` ada |
| `README.md:360` — UI "8.797 baris" | 15.463 baris |

---

## 7. Yang saya tidak verifikasi

Supaya tidak ada yang mengira ini lebih dari yang dikerjakan:

* Temuan P2 dan beberapa P1 berasal dari tinjauan per-lapisan dengan rujukan
  `file:line`; temuan P0 dan §3 nomor 3.1, 3.3, 3.4, 3.6, 3.7, 3.8 saya buka
  sendiri di sumbernya.
* Perilaku runtime (bukan pembacaan kode) hanya diverifikasi lewat gate tes: tidak
  ada brain yang dijalankan melawan model live, tidak ada node jarak jauh yang
  didial, dan tidak ada agen ACP nyata yang dipanggil dalam tinjauan ini.
* Angka tes (1240) dari `scripts/check.sh` penuh hari itu, termasuk `swarmbot-ui`.

---

## 8. Apa yang dikerjakan (dan apa yang tidak)

Seluruh tahap §5 dieksekusi. Ringkasannya, dengan bukti yang bisa diperiksa:

### Tahap 0 — kejujuran & keamanan

| Temuan | Perbaikan | Tes yang mengunci |
| --- | --- | --- |
| §2.1 satu "always allow" membatalkan lantai destruktif | `target_matches_prefix` cocok pada batas kata (`contracts/src/request.rs`); `evaluate` menurunkan `Allow` → `Ask` untuk perintah destruktif; `rule_from` mengembalikan `None` sehingga AllowAlways tidak pernah menulis aturan untuk perintah destruktif | `a_command_prefix_matches_on_a_word_boundary_only`, `a_saved_allow_rule_cannot_authorise_a_destructive_command`, `allow_always_on_a_destructive_command_writes_no_rule` |
| §2.2 `auto_approve_read_only` membatalkan `deny` | pemeriksaan dipindah ke cabang `Ask` **setelah** `evaluate` | `a_deny_rule_beats_auto_approve_read_only` |
| §2.3 `shell` di luar `PathPolicy` | `PathPolicy::denied_reference` memindai kata perintah untuk path di deny list; batas yang tidak tercakup ditulis di `docs/tools.md` | `a_shell_command_may_not_spell_out_a_denied_path`, `a_deny_listed_path_in_the_command_is_refused_before_it_runs`, `an_ordinary_command_still_runs_with_a_deny_list_configured` |
| §2.4 bot disable → tuli permanen | `set_enabled(false)` memakai `unregister` | `a_disabled_bot_loses_its_runner_and_gets_a_new_one_back` |
| §2.5 shutdown menggantung | `BusyGuard` (drop guard) + `notify_one` + flag `done` | `a_cancelled_turn_clears_the_roster_and_lets_shutdown_finish`, `joined_returns_when_the_runner_already_stopped` |
| §2.6 Stop tidak membatalkan kartu | `ApprovalGate::cancel_turn` (default no-op) diimplementasikan `ApprovalBridge` dan dipanggil jalur `Interrupt` | (tercakup jalur approval; lihat catatan di bawah) |
| §3.3 `/mcp` tanpa autentikasi | `/mcp` di belakang `require_auth`; `internal_addr` divalidasi harus loopback saat resolve | `internal_listener_refuses_an_anonymous_tools_call`, `a_non_loopback_internal_addr_is_refused`, `a_non_loopback_internal_addr_stops_the_brain_before_it_binds` |
| §3.10–3.12 audit/spill/mode | satu `write_all` per baris audit, 0600/0700, `write_file` mempertahankan mode + fsync direktori | `concurrent_appends_keep_one_json_object_per_line`, `a_spilled_result_is_created_private`, `a_spilled_stream_is_created_private` |

### Tahap 1 — menyalakan yang sudah 90% jadi

* **Routines menyala**: `start_routine_ticker()` dan `start_housekeeping_ticker()`
  dipanggil di brain dan CLI embedded; `settle_delivery` menutup `RoutineRun` saat
  turn selesai, dan `reap_stale_runs` menandai run yang dipotong crash.
  Tes: `a_fired_run_is_closed_when_its_turn_ends`, `a_failed_routine_turn_records_why`,
  `a_run_a_crash_cut_off_is_reaped_at_boot`, `housekeeping_reaps_a_routine_run_a_crash_cut_off`.
* **`events_log` dibatasi**: `LiveHub::truncate_log` dipanggil ticker housekeeping.
  Tes: `housekeeping_bounds_the_event_log`.
* **UI menampilkan kegagalan**: `render_top_bar` kini dipanggil dari `Root::render`
  (jalur `run_app`), dengan `top_bar_model` sebagai keputusan murni yang bisa diuji
  dan `TOAST_TTL` sebagai data. Tes: 9 tes di `state`/`reducer`/`view`/`worker`.
* **Exit code jujur**: `0/1/2/3/64` — `2` hanya timeout, `3` stream berakhir,
  `64` error pemakaian. Tes: `a_usage_error_is_never_the_timeout_code`,
  `a_remote_run_whose_stream_ends_is_not_reported_as_a_timeout`,
  `a_json_run_ends_with_one_object_that_names_the_stop_reason`, plus tes integrasi
  yang menjalankan binary sungguhan.

### Tahap 2 — M3 ditutup

`/mcp` statis berautentikasi owner, `/mcp/t/{token}` per-turn, dan `ClaudeCli`
benar-benar berjalan lewat `claude -p` + `--mcp-config`. `steer` dan
`survives_restart` dikoreksi menjadi `false` karena `chat()` tidak bisa melakukan
keduanya — capability yang berbohong diperbaiki dengan menurunkan klaim, bukan
menaikkan perilaku.

### Tahap 3 — penguatan

* **Store keluar dari reactor**: `ThreadedStore` (satu lane penulis, N lane pembaca,
  masing-masing thread + koneksi sendiri) mengimplementasikan seluruh 14 trait port,
  dan **diadopsi di kedua composition root** (`brain/src/compose.rs`,
  `core/src/compose.rs`). Batas jujurnya: panggilan port tetap sinkron, jadi
  pemanggil inline masih memarkir worker-nya sendiri; yang hilang adalah
  serialisasi proses-wide. Tes: 10 tes di `store/tests/threaded.rs`, dua di
  antaranya diverifikasi dengan mutasi.
* **Deadline per-call node**: `fs.*` 60 s, `shell.exec` = `timeoutMs` + 30 s,
  `node.hello` 10 s; kedaluwarsa menarik panggilan, mengirim `Interrupt`, dan
  menghasilkan error bertipe yang menyebut node. Tes: 4 di `mux.rs`, 3 di
  `executor.rs`, 1 di `session.rs`.
* **`JsonlMirror::rebuild` memulihkan conversation**: header per berkas membawa baris
  conversation, dan `last_seq` dipulihkan sehingga store hasil rebuild bisa
  di-append. Berkas lama tanpa header *diinferensi* dan dilaporkan sebagai
  `RebuildReport::inferred`. Tes: kontrak `contract_mirror`.
* **Stream terpotong ditolak**: `Malformed` bila tidak ada `[DONE]` maupun
  `finish_reason`; `LlmEvent::Restart` menandai batas percobaan dan renderer
  membuang teks percobaan yang gagal. Tes: 6 di `tests/openai_compat.rs`,
  `a_restart_discards_the_failed_attempts_streamed_text` di core.
* **Isolasi ACP**: timeout setup tidak lagi mematikan koneksi bersama; setelah tiga
  timeout berturut proses diganti sekali; `session/close` dikirim bila agen
  mengiklankannya. Tes: `one_slow_setup_fails_only_its_own_turn`,
  `a_stop_during_another_turns_handshake_returns_promptly`,
  `a_completed_turn_closes_its_acp_session`.

### Tahap 4 — M7 sebagian

* **`remember`**: tool baru yang menulis `MEMORY.md` (dedup, satu baris per fakta,
  memangkas baris tertua agar tetap di bawah budget prompt, menolak menulis lewat
  symlink). Tes: 5 di `bot_tools`.
* **Pustaka skill**: `<data>/skills/<nama>/SKILL.md` (atau `<nama>.md`) dibaca dan
  diinjeksi dengan cap per-skill dan per-turn; nama skill tidak bisa menjadi path.
  Tes: 5 di `instructions`.
* **Data stream `/node/stream/:id` dan `codex app-server`**: **tidak dikerjakan.**
  Keduanya tetap refusal bertipe, dan itu dinyatakan di README serta
  `docs/architecture.md`.

### Bug yang ditemukan saat mengerjakan

* **Runner yang panic hilang diam-diam.** Tokio menyimpan panic di `JoinHandle`
  yang tidak pernah di-`await`, sehingga task berhenti, bot tetap `Working`, turn
  tetap `Running`, dan tidak ada entri transkrip — sehingga setiap pemeriksaan idle
  melihat tim yang tenang dan melaporkan sukses. Ini kemungkinan besar akar flake
  `real_brain` yang terdokumentasi. Sekarang panic ditangkap, dicatat dengan nama
  bot, dan roster dibersihkan. Tes:
  `a_panicking_runner_is_reported_and_leaves_the_roster_idle`.
* **Tes `ThreadedStore` yang menggantung.** `a_panicking_call_stops_the_lane_and_later_calls_fail_typed`
  menunggu `recv()` tanpa batas pada channel yang sender-nya masih hidup, sehingga
  menggantung gate 36 menit alih-alih gagal. Sekarang semua penantian di tes itu
  dibatasi `recv_timeout`, dan kasus yang dijanjikan namanya (panggilan yang datang
  *setelah* lane mati) benar-benar diuji.

### Yang tetap tidak terverifikasi

* **Flake `real_brain` di bawah beban.** Saya reproduksi dengan 4–6 proses paralel:
  1–2 dari 20 run gagal dengan gejala yang sama. **Baseline `SqliteStore` juga
  gagal**, jadi ini bukan regresi `ThreadedStore`. Panic-runner yang kini terlihat
  menjelaskan satu mekanisme, tetapi saya **tidak** membuktikan flake itu hilang:
  gate penuh terakhir lulus, dan itu bukan bukti. Perbaikannya (panic yang terlihat)
  membuat kegagalan berikutnya bisa didiagnosis, bukan hilang.
* **UI secara visual.** `--selftest` berjalan di Wayland (10 jendela, exit 0), tetapi
  kontras, tinggi bar, dan titik pemotongan teks tidak bisa saya lihat tanpa GPU.
* **Node yang menggantung di handler sungguhan.** Deadline diuji melawan peer
  in-memory; `crates/node` membuktikan interrupt→`killpg` terpisah, tetapi tidak ada
  tes yang menyatukan keduanya.
* **`codex app-server` dan data stream** — tidak diimplementasikan, dan tidak ada
  klaim sebaliknya di kode maupun dokumen.

---

## 9. Tindak lanjut — November 2026 (putaran kedua)

Bagian ini mencatat apa yang dikerjakan **setelah** §8, dan apa yang tetap tidak
terbukti. Angka gate di §1–§8 adalah angka saat itu; yang berlaku sekarang ada di
[`README.md`](../README.md#status-milestone).

### Flake `real_brain` akhirnya terdiagnosis — dan bukan soal beban

§8 menutup dengan "saya tidak membuktikan flake itu hilang". Sekarang sebabnya
ditemukan, dan itu bukan starvation:

```
the refusal is a dial failure the node reports honestly:
2026-11-13T01:33:44.929401Z  INFO swarmbot-node starting …
```

Tes mencari needle `401`, dan **timestamp tracing** mengandung `401` (`.929401`).
`wait_for_line` mencocokkan substring ke baris mentah, jadi ia mengembalikan baris
startup daemon sebelum penolakan yang dimaksud sempat muncul. Makin banyak baris
log — makin sibuk mesinnya — makin besar peluang jam menjawab pertanyaan itu. Itu
sebabnya gejalanya tampak seperti sensitivitas beban, dan sebabnya ia tidak pernah
reproduksi saat dijalankan sendirian.

Perbaikan: `message_of(line)` membuang prefiks tracing sebelum mencocokkan, dan
`a_timestamp_is_never_mistaken_for_the_message` mengunci perilaku itu dengan
reproduksi yang deterministik.

**Koreksi jujur atas versi pertama bagian ini.** Setelah perbaikan itu saya menulis
"12 run berturut-turut hijau", dan itu benar untuk 12 run itu — tetapi bukan bukti
bahwa flake-nya hilang. Di bawah beban yang lebih keras (6 proses paralel) tes yang
sama masih gagal, dan penyebabnya **tiga hal berbeda yang kebetulan berbagi satu
gejala**:

1. **Needle `401` cocok dengan timestamp** (di atas). Diperbaiki.
2. **Antrean skrip `fake` dipakai bersama.** `Rig` membangun brain dengan
   `demo_team("fake")`, jadi empat bot demo dan bot yang diuji berbagi **satu**
   antrean `FakeBackend`. Di bawah beban, bot lain bisa mem-pop skrip yang ditulis
   untuk bot yang diuji; gejalanya transkrip hanya berisi pesan owner tanpa tool
   call, dengan exit code 0. Itu bentuk yang sama dengan D9.15 — dan `Rig`
   membangun brain-nya langsung, jadi ia tidak pernah melewati perbaikan D9.15.
   Sekarang bot yang diuji memakai backend sendiri (`fake-node`): antrean dibagi
   → antrean dipisah, bukan wait yang dilebarkan.
3. **Race roster: lost update yang nyata** (bagian berikut).

Sesudah (2) dan (3): 24/24 run hijau pada 4× paralel, 30 run pada 6× paralel
menyisakan 4 kegagalan yang **semuanya** berasal dari (3) — bukan dari dua yang
lain.

### Debt yang dibayar

| Debt | Perbaikan |
| --- | --- |
| `node/src/shell.rs` menyalin `exec_shell` | `tools::handlers::shell::exec_shell_streaming` sekarang publik (`ShellEvent`/`ShellEventSink`/`ShellOptions`/`ShellRun` ikut diekspor). `node/src/shell.rs` turun dari 1004 ke 283 baris: yang tersisa hanya adapter — policy node, spill dir, audit, dan pemetaan `ShellEvent` → `ShellChunk`. Daftar "enam hal yang harus sinkron" hilang. Batas yang jujur: **run loop**-nya tetap dua (buffered vs streaming), berdampingan di satu file, keduanya lewat `build_shell_command` yang sama — jadi process group, environment tersaring, dan timeout grup tidak bisa lagi berbeda antar crate |
| `spill/` di luar root default | `NodeConfig::default()`/`new_in()` sekarang punya dua root: `<data>/workspace` (rw, tetap pertama) dan `<data>/spill` (**ro**). Keluaran spill bisa dibaca balik; direktori itu tidak bisa dipakai menulis. Tes: `the_default_roots_are_the_workspace_then_a_read_only_spill_directory`, `a_spilled_file_can_be_read_back_but_the_spill_directory_is_not_writable` |
| `NodeExecutor::exec` tidak membawa `Invocation` (D9.12) | Port menjadi `exec(&self, node, &Invocation, call)`. `NodeDispatcher::attribute()` — pemindaian `running_turns()` yang jatuh ke sentinel `bot_unknown` begitu ada dua turn di satu node — **dihapus**. Atribusi sekarang fakta dari pemanggil. Tes `two_turns_on_one_node_each_carry_their_own_invocation_to_the_node` diverifikasi dengan mutasi: mengembalikan jalur lama membuatnya gagal dengan `left: "bot_unknown"` |

### Dua permukaan baru

* **Usage.** `Turn.usage` sudah dihitung dan dipersist sejak lama tetapi tidak ada
  yang bisa membacanya. Sekarang: `TurnRepo::turns_since`, `UsageReport`/`BotUsage`
  di `contracts`, `GET /api/usage?since=…` di brain, `BrainClient::usage` di client,
  dan `swarmbot usage [--since T] [--json]` di CLI. Aturan jendela (batas `since`
  inklusif, `until` sebagai batas atas yang ditegakkan, bot terhapus tetap punya
  baris tanpa nama) hidup di **satu** fungsi — `UsageReport::window` — karena dua
  front end menjawab pertanyaan yang sama dan yang menyimpang akan salah tanpa
  ketahuan. Batas jujur: angkanya adalah yang dilaporkan backend; backend yang diam
  tampak nol, dan `turns` ada di laporan supaya "tidak ada turn" bisa dibedakan dari
  "ada turn, tidak ada angka".
* **Backup.** `store::backup`: `VACUUM INTO` (bukan `cp` — halaman WAL yang sudah
  commit tidak terlihat oleh salinan file, dan SQLite yang robek tetap terbuka),
  transkrip ikut, `MANIFEST.json` ditulis **paling akhir**, dan `verify` menghitung
  ulang semuanya dari snapshot alih-alih memercayai manifest. `swarmbot backup
  create --out DIR` memverifikasi hasilnya sendiri sebelum melaporkan sukses, dan
  menolak `--brain` karena database-nya ada di mesin daemon.

### Bug produk yang ditemukan saat memburu flake — dan sudah diperbaiki

Versi pertama bagian ini menutup dengan "lost update di roster node tidak
diperbaiki, jangan percaya roster di bawah konkurensi". Itu benar saat ditulis, dan
ketiganya sekarang sudah diperbaiki. Urutannya penting, karena yang satu baru
terlihat setelah yang sebelumnya ditutup.

**1. Lost update di roster node.** `node.hello`, `node.heartbeat`, `node.event`,
perubahan presence dan revocation semuanya menulis **baris yang sama**, dan mux
men-spawn satu task per request masuk — jadi mereka benar-benar tumpang tindih,
bukan sekadar "bisa". Setiap penulis dulunya read–modify–write tangan:

```rust
let mut stored = store.get_node(node)?;   // baca
stored.policy = ...;                      // ubah satu field
store.update_node(&stored)?;              // tulis SELURUH baris, tanpa CAS
```

Heartbeat yang membaca sebelum hello menulis akan mengembalikan baris pra-hello,
jadi `policy` node yang sedang online kembali ke nilai lama dan brain merencanakan
auto-run terhadap ceiling yang bukan milik node itu. Perbaikan: `NodeInfo` kini
membawa `revision` (invarian yang sama dengan `Entry` — *a stale write must be a
conflict, not a silent overwrite*), `update_node` menjawab `DomainError::Conflict`
di kedua store, dan semua penulis gateway lewat `session::mutate_node` yang
membaca ulang dan menerapkan lagi saat conflict. Tes: interleaving lost-update di
contract suite `store` (jalan di keempat driver) dan
`a_conflicting_roster_write_is_retried_and_lands` — keduanya diverifikasi dengan
mutasi.

**2. `wait_for_idle` berbohong di awal setiap turn.** Setelah (1) ditutup, tes
end-to-end masih gagal 3/36 di bawah beban, dengan transkrip yang hanya berisi
pesan owner. Diagnostik turn yang saya tambahkan menyebut penyebabnya:
`status=Running, llm_calls=0`. Artinya turn **sedang berjalan** saat `settle()`
melaporkan idle. Ada jendela di awal turn — delivery sudah keluar dari antrean,
runner belum menandai dirinya busy — di mana keempat sinyal in-memory (`busy`,
`queued`, `tasks`, `parked_count`) sama-sama membaca "tidak ada apa-apa".
`wait_for_idle` sekarang juga membaca `store.running_turns()`: baris di store
adalah satu-satunya sinyal yang benar sejak turn dibuat. Ini bukan cuma soal tes —
`swarmbot run --wait` memakai fungsi yang sama, jadi sebelumnya ia bisa keluar 0
sambil mengklaim tim sudah selesai padahal belum.

**3. Runner yang panic meninggalkan turn terbuka selamanya.** Konsekuensi dari (2):
jalur panic membersihkan flag `busy` tetapi tidak menutup turn, jadi turn tetap
`Running` dan `wait_for_idle` akan menghabiskan seluruh budget-nya untuk kerja
yang sudah mati. Sekarang turn bot itu ditutup sebagai `Failed` dengan alasan
"the bot runner panicked: …".

**4. Hello bisa membatalkan revocation.** Ditemukan oleh tes yang saya tulis untuk
(1): `apply_hello` menulis `stored.revoked = false`. Upgrade memang menolak node
revoked, tapi bila revocation mendarat *setelah* upgrade diverifikasi dan hello
masih di jalan, hello itu menghidupkan kembali node yang baru saja diputus
pemiliknya — dan karena `NodeDispatcher` merutekan berdasarkan baris itu, brain
akan mengirim tool call lagi ke mesin yang sudah dicabut. Pemeriksaannya sekarang
ada **di dalam** loop yang dijaga CAS (bukan sebelum atau sesudah tulis), jadi
tidak ada jendela: bila revoke mendarat di antaranya, tulisannya conflict, loop
membaca ulang, veto melihat `revoked`, dan tidak ada yang ditulis sama sekali.
Socket ditutup dengan 4401. Tes `a_hello_for_a_revoked_node_does_not_unrevoke_it`
mengunci revision tidak berubah — bukti terkuat bahwa hello yang ditolak tidak
menyentuh barisnya.

Pelajaran dari keempatnya, dan alasan saya menuliskannya: **gejala "flaky di bawah
beban" adalah empat bug berbeda yang berbagi satu gejala.** Dua di antaranya
(2 dan 4) adalah bug produk yang bisa terlihat pemilik, bukan kelemahan tes. Yang
membuatnya ketemu bukan menjalankan tes berulang kali, tetapi membuat kegagalannya
menyebut keadaan yang sebenarnya — satu `turn_report()` di pesan assertion
mengubah "transkrip kosong" menjadi "turn masih Running, 0 llm call".

### Yang tetap tidak terverifikasi setelah putaran ini

* **Adapter sungguhan mengisi `Usage`.** Semua angka yang pernah saya lihat berasal
  dari `FakeBackend::with_usage`. Apakah `openai`, `acp` dan `claude-cli` benar-benar
  mengisi `Usage` belum diuji terhadap model nyata — laporan hanya menampilkan apa
  yang ada di `Turn.usage`.
* **Panel usage di UI.** Route dan CLI ada; GPUI belum menampilkannya.
* **Restore ke brain yang sedang berjalan.** Yang terbukti: snapshot bisa dibuka
  sebagai store yang berfungsi dan berisi data pemilik. Prosedur "taruh kembali
  sebagai `<data>/swarmbot.sqlite` sambil brain hidup" tidak diuji.
* **`since` setahun.** `GET /api/usage` membaca seluruh jendela tanpa paginasi; aman
  untuk default 24 jam, tidak dibuktikan untuk jendela sangat panjang.
* **TLS, dua-proses-dengan-model-nyata, UI di GPU nyata, `codex app-server`, data
  stream** — semuanya tetap seperti di §8: belum.

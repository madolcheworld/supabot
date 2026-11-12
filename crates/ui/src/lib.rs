//! SwarmBot desktop UI — the GPUI app.
//!
//! One window: a left sidebar (roster, conversations, nodes), a main pane (a
//! transcript, cards and a composer, or the nodes/settings screens), and a
//! toggleable delegation-graph panel on the right. It is a **pure client** of the
//! brain's REST + `/ws/live` API (D7.1/D7.2): the main thread runs GPUI and
//! nothing else, and two blocking worker threads own all I/O.
//!
//! Module map:
//!
//! * [`args`] — the command line.
//! * [`theme`] — colours, spacing, the avatar-tile derivation.
//! * [`actions`] — GPUI actions and key bindings.
//! * [`state`] — [`state::AppState`], the plain-data source of truth.
//! * [`reducer`] — the pure [`reducer::apply`] that folds a [`swarmbot_contracts::LiveEvent`]
//!   (or a command result) into the state. **The correctness lives here.**
//! * [`proto`] — the [`proto::Command`] / [`proto::WorkerMsg`] values that cross
//!   the thread boundary.
//! * [`brain`] — the [`brain::Brain`] seam over [`swarmbot_client::BrainClient`].
//! * [`worker`] — the live + rpc threads, reconnect backoff, and the main-thread
//!   pump that applies [`proto::WorkerMsg`]s through the reducer.
//! * [`spawn`] — `--spawn-brain` / `--offline` child supervision.
//! * [`widgets`] — the text input, the markdown renderer, small shared bits.
//! * [`view`] — the screens: root layout, sidebar, conversation, composer, graph,
//!   cards, nodes, settings, palette.
//! * [`selftest`] — the fabricated-event driver behind `--selftest`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use gpui::{
    App, AppContext, Application, Bounds, Menu, MenuItem, SharedString, WindowBounds,
    WindowOptions, px, size,
};
use swarmbot_client::{BackendModels, BackendView, BrainClient, ClientError, ModelOption};

pub mod actions;
pub mod args;
pub mod brain;
pub mod proto;
pub mod reducer;
pub mod selftest;
pub mod spawn;
pub mod state;
pub mod theme;
pub mod view;
pub mod widgets;
pub mod worker;

use args::Args;
use brain::ClientBrain;
use proto::SecretToken;
use state::AppState;

/// How many times a root view rendered — `--selftest`'s hard assertion, and the
/// machine-checkable proof that windows opened and painted on this compositor.
pub static RENDERS: AtomicUsize = AtomicUsize::new(0);

/// A tiny append-only log the run writes to stdout and, with `--report`, to a
/// file. Kept as an `Arc<Mutex<..>>` because the GPUI quit callback and the
/// post-run flush both touch it.
#[derive(Clone, Default)]
pub struct Report(Arc<Mutex<Vec<String>>>);

impl Report {
    pub fn note(&self, line: impl Into<String>) {
        let line = line.into();
        println!("{line}");
        if let Ok(mut lines) = self.0.lock() {
            lines.push(line);
        }
    }

    pub fn flush(&self, path: Option<&str>) {
        let Some(path) = path else { return };
        let Ok(lines) = self.0.lock() else { return };
        if let Err(err) = std::fs::write(path, lines.join("\n") + "\n") {
            eprintln!("cannot write the report to {path}: {err}");
        }
    }
}

/// Parse argv, run the app, and return the process exit code. `main.rs` is a
/// one-line wrapper around this so the whole entry path is testable.
pub fn run() -> i32 {
    let args = Args::parse();
    let report = Report::default();
    report.note(format!(
        "swarmbot-ui starting (brain {}, offline={}, spawn={})",
        args.brain,
        args.offline,
        args.should_spawn()
    ));

    // Supervise a child brain if asked. The guard lives until `run` returns, so
    // the child is killed on the way out.
    let mut spawned: Option<spawn::SpawnedBrain> = None;
    if args.should_spawn() {
        match spawn::spawn(&args) {
            Ok(brain) => {
                report.note(format!(
                    "spawned swarmbot-brain pid={} argv=[{}]",
                    brain.id(),
                    brain.argv.join(" ")
                ));
                spawned = Some(brain);
            }
            Err(err) => {
                report.note(format!("SPAWN FAILED: {err}"));
                // Without a brain there is nothing to show; be honest and leave.
                report.flush(args.report.as_deref());
                return 1;
            }
        }
    }

    // Build the shared client. The resolved token sets the bearer used by both
    // REST and the WebSocket upgrade, and is exchanged for a session on the
    // worker (`Command::Login`).
    let client = match BrainClient::new(&args.brain) {
        Ok(client) => client,
        Err(err) => {
            report.note(format!("cannot build a client for {}: {err}", args.brain));
            report.flush(args.report.as_deref());
            return 1;
        }
    };
    // Resolve the credential once per run, after the spawned child is healthy
    // (the brain mints `<data-dir>/owner-token` during boot): `--token`, then
    // `SWARMBOT_TOKEN`, then the owner-token file in a data dir this process
    // was told about, else nothing — the app shows its login prompt on a 401.
    let env_token = std::env::var("SWARMBOT_TOKEN").ok();
    let data_dir: Option<PathBuf> = spawned
        .as_ref()
        .map(|child| child.data_dir().to_path_buf())
        .or_else(|| args.data_dir.as_ref().map(PathBuf::from));
    // A child this process spawned writes the file during boot, essentially
    // always before `/api/health` answers — but the zero-credential demo does
    // not bet on "essentially", so that one case gets a short bounded re-read.
    let budget = if spawned.is_some() {
        OwnerTokenBudget::SPAWNED
    } else {
        OwnerTokenBudget::SINGLE
    };
    let token = resolve_token(
        args.token.as_ref(),
        env_token.as_deref(),
        data_dir.as_deref(),
        budget,
        &report,
    );
    let client = match &token {
        Some(token) => client.with_token(token.expose()),
        None => client,
    };
    // When we started the brain ourselves, confirm we can actually authenticate
    // to it with the resolved token — a supervisor that reports "spawned" for a
    // brain it then cannot talk to would be lying. `wait_healthy` only proved the
    // child serves HTTP (a 401 counts as up); this proves the credential works.
    if spawned.is_some() {
        match client.health() {
            Ok(health) => report.note(format!(
                "brain reachable and authenticated (version {}, epoch {})",
                health.version, health.epoch
            )),
            Err(ClientError::Unauthorized) => report.note(
                "brain is up but this client is NOT authenticated with the resolved token; \
                 the login prompt will show",
            ),
            Err(err) => report.note(format!("brain health check after spawn failed: {err}")),
        }
    }
    let login_token = token;
    let brain = Arc::new(ClientBrain::new(client));

    let brain_url = SharedString::from(args.brain.clone());
    let selftest = args.selftest;
    let report_window = report.clone();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Application::new().run(move |cx: &mut App| {
            actions::bind_keys(cx);
            cx.set_menus(vec![Menu {
                name: "SwarmBot".into(),
                items: vec![
                    MenuItem::action("Toggle delegation graph", actions::ToggleGraph),
                    MenuItem::action("Command palette", actions::TogglePalette),
                    MenuItem::separator(),
                    MenuItem::action("Quit", actions::Quit),
                ],
            }]);
            cx.on_action(|_: &actions::Quit, cx: &mut App| cx.quit());

            if selftest {
                run_selftest(cx, brain_url, report_window);
            } else {
                run_app(cx, brain_url, data_dir, brain, login_token, report_window);
            }
        })
    }));

    let renders = RENDERS.load(Ordering::SeqCst);
    let ok = result.is_ok() && (renders > 0 || !args.selftest);
    report.note(format!(
        "exit: renders={renders} selftest={} ok={ok}",
        args.selftest
    ));
    report.flush(args.report.as_deref());
    drop(spawned);
    if ok { 0 } else { 1 }
}

/// Normal mode: start the worker threads, open one window, focus the composer.
fn run_app(
    cx: &mut App,
    brain_url: SharedString,
    data_dir: Option<PathBuf>,
    brain: Arc<ClientBrain>,
    login_token: Option<SecretToken>,
    report: Report,
) {
    let state = cx.new(|_| {
        let mut s = AppState::new(brain_url.to_string());
        s.data_dir = data_dir;
        s
    });
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let worker = worker::Worker::start(brain, events_tx);
    if let Some(token) = login_token {
        let _ = worker.commands.send(proto::Command::Login(token));
    }
    let _ = worker.commands.send(proto::Command::FetchBackends);
    worker::spawn_pump(cx, state.clone(), events_rx);
    worker::spawn_ticker(cx, state.clone());
    let commands = worker.commands.clone();

    let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |_window, cx| cx.new(move |cx| view::Root::new(state, commands, Some(worker), cx)),
    );
    match opened {
        Ok(window) => {
            report.note("window opened; rendering");
            let _ = window.update(cx, |root, window, cx| root.focus_default(window, cx));
        }
        Err(err) => {
            report.note(format!("WINDOW FAILED: {err}"));
            cx.quit();
        }
    }
    cx.activate(true);
}

/// `--selftest`: drive fabricated events through the reducer, then open **one
/// window per screen**. `open_window` forces a synchronous draw before it
/// returns, so each screen's render path actually runs and is counted — real
/// per-screen coverage on the live compositor, which a headless test cannot give
/// here (GPUI's `TestAppContext` is behind `test-support`, which pulls in `x11`
/// and would not link on this machine).
fn run_selftest(cx: &mut App, brain_url: SharedString, report: Report) {
    let configs = selftest::screen_configs();
    let windows = configs.len();
    let mut applied = 0usize;
    let mut failed = false;
    for (index, cfg) in configs.into_iter().enumerate() {
        let state = cx.new(|_| AppState::new(brain_url.to_string()));
        applied = selftest::drive(cx, &state);
        state.update(cx, |s, _| {
            s.screen = cfg.screen;
            s.graph_open = cfg.graph;
            s.palette_open = cfg.palette;
            if cfg.plus_menu {
                s.plus_menu_open = true;
            }
            if cfg.create_bot {
                s.open_create_bot();
            }
            if cfg.create_group {
                s.open_create_group();
            }
            if cfg.provider_menu {
                // Fabricate what the REST fetches would have delivered, so
                // the menu's two columns have real rows to render.
                s.backends = vec![
                    BackendView {
                        id: "codex".into(),
                        kind: swarmbot_contracts::BackendKind::Acp,
                        base_url: None,
                        model: None,
                        api_key_env: None,
                        options: Default::default(),
                    },
                    BackendView {
                        id: "fake".into(),
                        kind: swarmbot_contracts::BackendKind::Fake,
                        base_url: None,
                        model: Some("fake".into()),
                        api_key_env: None,
                        options: Default::default(),
                    },
                ];
                s.models.insert(
                    "codex".into(),
                    Ok(BackendModels {
                        models: vec![
                            ModelOption {
                                id: "gpt-6-luna[high]".into(),
                                label: None,
                            },
                            ModelOption {
                                id: "gpt-5.6-terra[medium]".into(),
                                label: None,
                            },
                        ],
                        authoritative: true,
                    }),
                );
                s.menu_backend = Some("codex".into());
                s.bot_menu_open = true;
            }
            if cfg.screen == state::Screen::Routines
                && let Ok(r) = swarmbot_contracts::Routine::new(
                    swarmbot_contracts::BotId::from_suffix("lead"),
                    "Daily Triage",
                    "review pending PRs and summarize",
                    swarmbot_contracts::RoutineSchedule::Interval { every_secs: 300 },
                    chrono::Utc::now(),
                )
            {
                s.routines = vec![r];
                s.routine_creator_open = true;
            }
        });
        // A sink so the views can send commands; the receiver is dropped, so a
        // send simply fails and nothing reaches a brain (there is none).
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                show: index == 0,
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| view::Root::new(state.clone(), tx.clone(), None, cx)),
        );
        match opened {
            Ok(_) => report.note(format!(
                "selftest: window {index} drew screen={:?} graph={} palette={} provider_menu={} plus_menu={} create_bot={} create_group={}",
                cfg.screen,
                cfg.graph,
                cfg.palette,
                cfg.provider_menu,
                cfg.plus_menu,
                cfg.create_bot,
                cfg.create_group
            )),
            Err(err) => {
                report.note(format!("WINDOW FAILED (screen {index}): {err}"));
                failed = true;
            }
        }
    }
    report.note(format!(
        "selftest: applied {applied} fabricated live event(s) through the reducer; opened {windows} window(s)"
    ));

    let report_quit = report.clone();
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(400))
            .await;
        let renders = RENDERS.load(Ordering::SeqCst);
        report_quit.note(format!(
            "selftest: {renders} total render(s) across every screen"
        ));
        report_quit.note(format!("vulkan/renderer: {}", renderer_summary()));
        let _ = cx.update(|cx| cx.quit());
    })
    .detach();
    if failed {
        // A window that failed to open is a hard failure; leave now so the exit
        // code reflects it rather than waiting out the timer.
        cx.quit();
    }
    cx.activate(true);
}

/// How hard to look for `<data-dir>/owner-token`. A single read is right for a
/// directory that merely exists; a child *this process spawned* writes the file
/// during boot — essentially always before `/api/health` answers, but the
/// supervisor does not bet the zero-credential demo on "essentially". The retry
/// is a short bounded budget (a handful of attempts over a few hundred ms),
/// never a poll loop: if the file is not there after that, it is not coming,
/// and the login prompt is the honest fallback.
#[derive(Clone, Copy, Debug)]
struct OwnerTokenBudget {
    attempts: usize,
    delay: Duration,
}

impl OwnerTokenBudget {
    /// One read, no waiting: a `--data-dir` this process did not spawn into has
    /// no boot race to cover.
    const SINGLE: Self = Self {
        attempts: 1,
        delay: Duration::ZERO,
    };
    /// A few quick re-reads for the spawned child's data dir (~360 ms worst
    /// case, all before the first window opens).
    const SPAWNED: Self = Self {
        attempts: 4,
        delay: Duration::from_millis(120),
    };
}

/// Resolve the credential to authenticate with, in strict precedence:
///
/// 1. `--token`;
/// 2. `SWARMBOT_TOKEN`;
/// 3. `<data-dir>/owner-token` — the one-time owner token the brain mints at
///    boot — read **only** when the data dir is one this process was told
///    about: the child it spawned, or an explicit `--data-dir`. This is what
///    makes `swarmbot-ui --offline` a genuine zero-credential demo: the
///    supervisor already knows where the brain wrote the file;
/// 4. nothing — the app then shows its in-window login prompt on a 401.
///
/// A whitespace-only `--token`/`SWARMBOT_TOKEN` counts as absent, and so does
/// an empty, missing or unreadable owner-token file: every failed source falls
/// through silently to the next. The token **value** is never logged — only the
/// source that won, or, when nothing was found, one honest line naming every
/// source and path tried (a user staring at the login prompt should learn where
/// the CLI already looked, including that a remote `--brain` with no local dir
/// leaves nothing to read).
fn resolve_token(
    flag: Option<&SecretToken>,
    env: Option<&str>,
    data_dir: Option<&Path>,
    budget: OwnerTokenBudget,
    report: &Report,
) -> Option<SecretToken> {
    if let Some(token) = flag.filter(|t| !t.expose().trim().is_empty()) {
        report.note("token: from --token");
        return Some(token.clone());
    }
    if let Some(token) = env.filter(|t| !t.trim().is_empty()) {
        report.note("token: from SWARMBOT_TOKEN");
        return Some(SecretToken::new(token));
    }
    if let Some(dir) = data_dir {
        let mut last_error = None;
        for attempt in 0..budget.attempts.max(1) {
            if attempt > 0 {
                std::thread::sleep(budget.delay);
            }
            match spawn::read_owner_token(dir) {
                Ok((path, token)) => {
                    // The source is public information; the value is not.
                    report.note(format!("token: {}", path.display()));
                    return Some(SecretToken::new(token));
                }
                Err(err) => last_error = Some(err),
            }
        }
        // `read_owner_token` errors name the path they tried, so this line
        // satisfies "the honest message must name the path(s) tried".
        let why = last_error
            .map(|err| err.to_string())
            .unwrap_or_else(|| "the file never appeared".to_string());
        report.note(format!(
            "token: none found — tried --token, SWARMBOT_TOKEN, and the owner token file ({why}); \
             the window will show a login prompt if the brain needs one"
        ));
        return None;
    }
    report.note(
        "token: none found — tried --token and SWARMBOT_TOKEN, and this process knows no data \
         dir to read owner-token from (a remote --brain keeps its token to itself); the window \
         will show a login prompt if the brain needs one",
    );
    None
}

/// What GPUI is rendering with. GPUI does not expose the adapter name through
/// `App`, so the honest summary is the ICD the loader was pointed at, which
/// `vulkaninfo --summary` also reports.
fn renderer_summary() -> String {
    std::env::var("VK_ICD_FILENAMES")
        .map(|icd| format!("icd={icd}"))
        .unwrap_or_else(|_| "icd=default".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A hand-rolled temp dir (the crate cannot add `tempfile`), removed on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "swarmbot-ui-token-{}-{}-{}",
                tag,
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&path).expect("create the temp dir");
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        /// Where the brain would mint its owner token inside this dir.
        fn owner_token(&self) -> PathBuf {
            self.0.join("owner-token")
        }
        fn write_owner_token(&self, contents: &str) {
            std::fs::write(self.owner_token(), contents).expect("write the token file");
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A budget shaped like [`OwnerTokenBudget::SPAWNED`] but with no delay, so
    /// the re-read loop is exercised without sleeping: the real budget only
    /// adds the sleeps between the same attempts.
    const INSTANT_RETRY: OwnerTokenBudget = OwnerTokenBudget {
        attempts: 4,
        delay: Duration::ZERO,
    };

    /// Everything the resolver logged — the test-side view of what a user sees.
    fn logged(report: &Report) -> String {
        report.0.lock().expect("report lock").join("\n")
    }

    fn args_with(token: Option<SecretToken>) -> Args {
        Args {
            brain: "http://127.0.0.1:1".to_string(),
            token,
            spawn_brain: false,
            offline: false,
            data_dir: None,
            brain_addr: None,
            selftest: false,
            report: None,
        }
    }

    #[test]
    fn the_flag_token_beats_the_owner_token_file() {
        let dir = TempDir::new("flag-wins");
        dir.write_owner_token("file-token\n");
        let report = Report::default();
        let flag = SecretToken::new("flag-token");
        let got = resolve_token(
            Some(&flag),
            Some("env-token"),
            Some(dir.path()),
            OwnerTokenBudget::SINGLE,
            &report,
        );
        assert_eq!(
            got.as_ref().map(SecretToken::expose),
            Some("flag-token"),
            "--token is source 1: above both the env var and the file"
        );
        let logged = logged(&report);
        assert!(
            logged.contains("--token"),
            "the winning source is named: {logged}"
        );
        for value in ["flag-token", "env-token", "file-token"] {
            assert!(
                !logged.contains(value),
                "no token value is ever logged: {logged}"
            );
        }
    }

    #[test]
    fn the_env_token_beats_the_owner_token_file() {
        let dir = TempDir::new("env-wins");
        dir.write_owner_token("file-token\n");
        let report = Report::default();
        let got = resolve_token(
            None,
            Some("env-token"),
            Some(dir.path()),
            OwnerTokenBudget::SINGLE,
            &report,
        );
        assert_eq!(
            got.as_ref().map(SecretToken::expose),
            Some("env-token"),
            "SWARMBOT_TOKEN is source 2: above the file, below --token"
        );
        let logged = logged(&report);
        assert!(logged.contains("SWARMBOT_TOKEN"), "{logged}");
        assert!(!logged.contains("env-token"), "{logged}");
        assert!(!logged.contains("file-token"), "{logged}");
    }

    #[test]
    fn discovers_the_owner_token_from_a_known_data_dir() {
        let dir = TempDir::new("discover");
        // Hand-written with a trailing newline, exactly like the brain mints it.
        dir.write_owner_token("minted-token\n");
        let report = Report::default();
        // This is the value `run()` hands to `BrainClient::with_token`, so a
        // spawned/known brain authenticates with no credential the user typed.
        let got = resolve_token(
            None,
            None,
            Some(dir.path()),
            OwnerTokenBudget::SINGLE,
            &report,
        );
        assert_eq!(
            got.as_ref().map(SecretToken::expose),
            Some("minted-token"),
            "the trailing newline is trimmed"
        );
        let logged = logged(&report);
        let path = dir.owner_token().display().to_string();
        assert!(
            logged.contains(&path),
            "the source is recorded as that path: {logged}"
        );
        assert!(
            !logged.contains("minted-token"),
            "the value is not: {logged}"
        );
    }

    #[test]
    fn an_absent_owner_token_falls_through_to_an_honest_line_naming_the_path() {
        let dir = TempDir::new("absent");
        let report = Report::default();
        // A known data dir with no owner-token file: the re-read budget runs
        // out and resolution falls through to the in-window login prompt.
        let got = resolve_token(None, None, Some(dir.path()), INSTANT_RETRY, &report);
        assert!(got.is_none(), "no token anywhere -> the login prompt");
        let logged = logged(&report);
        let path = dir.owner_token().display().to_string();
        assert!(
            logged.contains(&path),
            "the honest line names the path tried: {logged}"
        );
        assert!(
            logged.contains("--token") && logged.contains("SWARMBOT_TOKEN"),
            "and every other source tried: {logged}"
        );
        assert!(logged.contains("login prompt"), "{logged}");
    }

    #[test]
    fn a_remote_brain_with_no_local_dir_says_there_was_nothing_to_read() {
        let report = Report::default();
        assert!(
            resolve_token(None, None, None, OwnerTokenBudget::SINGLE, &report).is_none(),
            "no data dir this process was told about -> no file source at all"
        );
        let logged = logged(&report);
        assert!(
            logged.contains("no data"),
            "the honest line states the gap instead of failing silently: {logged}"
        );
        assert!(logged.contains("login prompt"), "{logged}");
    }

    #[test]
    fn an_empty_owner_token_file_is_ignored() {
        let dir = TempDir::new("empty-file");
        dir.write_owner_token("   \n");
        let report = Report::default();
        let got = resolve_token(
            None,
            None,
            Some(dir.path()),
            OwnerTokenBudget::SINGLE,
            &report,
        );
        assert!(got.is_none(), "an empty file is not a token");
        let logged = logged(&report);
        assert!(
            logged.contains("empty"),
            "the honest line says why: {logged}"
        );
    }

    #[test]
    fn whitespace_only_flag_and_env_fall_through_to_the_file() {
        let dir = TempDir::new("blank");
        dir.write_owner_token("minted-token\n");
        let report = Report::default();
        let blank = SecretToken::new("   ");
        let got = resolve_token(
            Some(&blank),
            Some("  "),
            Some(dir.path()),
            OwnerTokenBudget::SINGLE,
            &report,
        );
        assert_eq!(
            got.as_ref().map(SecretToken::expose),
            Some("minted-token"),
            "a whitespace-only credential counts as absent"
        );
    }

    #[test]
    fn the_retry_budget_is_bounded_when_the_file_never_appears() {
        // The re-read loop must terminate — a bounded retry, not a poll loop.
        // The zero-delay budget runs it instantly; the real `SPAWNED` budget
        // only adds ~360 ms of sleeps, all before the first window opens.
        let dir = TempDir::new("retry");
        let report = Report::default();
        let got = resolve_token(None, None, Some(dir.path()), INSTANT_RETRY, &report);
        assert!(got.is_none());
        // The real budget stays a handful of attempts over a few hundred ms —
        // checked at compile time, so growing it into a poll loop breaks the
        // build instead of the demo.
        const {
            assert!(
                OwnerTokenBudget::SPAWNED.attempts <= 8,
                "a handful of attempts, not a poll loop"
            );
            assert!(
                OwnerTokenBudget::SPAWNED
                    .delay
                    .saturating_mul(OwnerTokenBudget::SPAWNED.attempts as u32)
                    .as_millis()
                    <= 1000,
                "a few hundred ms in total"
            );
        };
    }

    #[test]
    fn the_token_value_is_never_logged_only_its_source() {
        let dir = TempDir::new("redact");
        dir.write_owner_token("super-secret-value\n");
        let report = Report::default();
        let _ = resolve_token(
            None,
            None,
            Some(dir.path()),
            OwnerTokenBudget::SINGLE,
            &report,
        );
        let logged = logged(&report);
        assert!(
            !logged.contains("super-secret-value"),
            "the token must never be logged: {logged}"
        );
        assert!(
            logged.contains("owner-token"),
            "the source path is logged instead: {logged}"
        );
    }

    #[test]
    fn no_debug_impl_leaks_a_token() {
        // Every type that can hold a credential redacts it in `Debug` — the
        // workspace rule for secrets (cf. the brain's `OwnerAuth`, the client's
        // `Http`). `Command`s appear in panic messages and test assertions,
        // `Args` in clap tooling; neither may print the value.
        let secret = "super-secret-value";
        let token = SecretToken::new(secret);
        let debugs = [
            format!("{token:?}"),
            format!("{:?}", Command::Login(token.clone())),
            format!("{:?}", args_with(Some(token))),
        ];
        for debug in &debugs {
            assert!(!debug.contains(secret), "Debug leaked the token: {debug}");
            assert!(
                debug.contains("redacted"),
                "Debug says what it did: {debug}"
            );
        }
    }
}

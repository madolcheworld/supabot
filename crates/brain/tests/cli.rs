//! The binary itself: the boot sequence, the one-time login URL, the exit codes.
//!
//! Everything else in this crate's tests builds an [`swarmbot_brain::AppState`] in
//! process. These tests run the real executable, because three things only exist
//! there: the argument parsing, the order the boot steps happen in (a token file
//! that is written *after* a listener opens would be a window where nobody can log
//! in), and the process's exit code.

mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;

/// The daemon under test. Built by cargo, so `CARGO_BIN_EXE_` is set.
const BIN: &str = env!("CARGO_BIN_EXE_swarmbot-brain");

/// A command that runs against a private data dir.
///
/// No `--offline` here on purpose: several of these tests are about what a config
/// file *says*, and `--offline` overrides the backend list. The daemon test adds
/// it explicitly, because that one must not be able to reach a model provider.
fn brain_command(data_dir: &std::path::Path) -> Command {
    let mut command = Command::new(BIN);
    command
        .arg("--data-dir")
        .arg(data_dir)
        // Keep the child's log out of the test's output unless a test asks for it.
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

/// Run a one-shot flag and return `(exit code, stdout and stderr together)`.
///
/// A failure is reported on stderr and everything else on stdout; a test that
/// asserts on what an operator would read wants both.
async fn run_once(args: &[&str], data_dir: &std::path::Path) -> (Option<i32>, String) {
    let output = brain_command(data_dir)
        .args(args)
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("the binary runs");
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code(), text)
}

/// The demo roster that ships with the repository.
fn team_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/team.example.toml")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn print_config_renders_the_resolved_configuration_without_a_secret() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::write(
        dir.path().join("secrets.json"),
        "{\"INFERHUB_API_KEY\":\"sk-super-secret-value\"}",
    )
    .expect("write");
    std::fs::write(
        dir.path().join("config.toml"),
        r#"
        default_backend = "inferhub"

        [[backends]]
        id = "inferhub"
        kind = "openai_compat"
        base_url = "https://api.inferhub.dev/v1"
        api_key_env = "INFERHUB_API_KEY"
        model = "ali/qwen3.8-flash"
        "#,
    )
    .expect("write");

    let (code, stdout) = run_once(&["--print-config"], dir.path()).await;
    assert_eq!(code, Some(0), "{stdout}");

    // The JSON is the last thing on stdout, after the log lines.
    let rendered = stdout
        .lines()
        .skip_while(|line| !line.starts_with('{'))
        .collect::<Vec<_>>()
        .join("\n");
    let config: Value = serde_json::from_str(&rendered)
        .unwrap_or_else(|err| panic!("--print-config did not print JSON ({err}):\n{stdout}"));
    assert_eq!(config["default_backend"], "inferhub");
    assert_eq!(config["offline"], Value::Bool(false));
    assert_eq!(config["expiry_ticker"], Value::Bool(true));
    assert!(config["layout"]["database"].is_string(), "{config}");

    // The invariant that matters: the key is in `secrets.json`, and the printed
    // configuration names only the variable it comes from.
    assert!(!stdout.contains("sk-super-secret-value"), "{stdout}");
    let backends = config["backends"].as_array().expect("backends");
    let inferhub = backends
        .iter()
        .find(|backend| backend["id"] == "inferhub")
        .expect("the configured backend");
    assert_eq!(inferhub["api_key_env"], "INFERHUB_API_KEY");
    assert!(inferhub.get("api_key").is_none(), "{inferhub}");
    // The documented spelling of the kind survives the round trip.
    assert_eq!(inferhub["kind"], "open_ai_compat");

    // A data dir the flags override shows up in the rendering.
    let (code, stdout) = run_once(
        &["--print-config", "--port", "9999", "--no-ticker"],
        dir.path(),
    )
    .await;
    assert_eq!(code, Some(0), "{stdout}");
    assert!(stdout.contains("127.0.0.1:9999"), "{stdout}");
    assert!(stdout.contains("\"expiry_ticker\": false"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn check_validates_the_configuration_and_opens_the_store() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (code, stdout) = run_once(
        &["--check", "--team", &team_file().to_string_lossy()],
        dir.path(),
    )
    .await;
    assert_eq!(code, Some(0), "{stdout}");
    assert!(stdout.contains("configuration is valid"), "{stdout}");
    // `--check` opens the store, so the file exists afterwards — and that is the
    // point: a data dir that cannot hold a database is worth finding out now.
    assert!(dir.path().join("swarmbot.sqlite").exists(), "{stdout}");
    // It does not mint a token: nothing was served.
    assert!(!dir.path().join("owner-token").exists(), "{stdout}");

    // A config that names a backend it does not define fails, and says why.
    std::fs::write(
        dir.path().join("config.toml"),
        "default_backend = \"inferhub\"\n\n[[backends]]\nid = \"fake\"\nkind = \"fake\"\n",
    )
    .expect("write");
    let (code, output) = run_once(&["--check"], dir.path()).await;
    assert_ne!(code, Some(0));
    assert!(output.contains("default_backend"), "{output}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broken_config_file_is_reported_with_its_line_number() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::write(
        dir.path().join("config.toml"),
        "default_backend = \"fake\"\nthis is not toml\n",
    )
    .expect("write");
    let output = brain_command(dir.path())
        .arg("--check")
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("the binary runs");
    assert_ne!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("config.toml"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn help_and_version_describe_every_flag() {
    let dir = tempfile::tempdir().expect("a temp dir");
    for (flag, wanted) in [
        ("--help", "swarmbot-brain"),
        ("--version", env!("CARGO_PKG_VERSION")),
    ] {
        let output = Command::new(BIN)
            .arg(flag)
            .current_dir(dir.path())
            .output()
            .await
            .expect("the binary runs");
        assert!(output.status.success(), "{flag} failed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(wanted),
            "{flag} did not mention {wanted}:\n{stdout}"
        );
    }
    let output = Command::new(BIN)
        .arg("--help")
        .current_dir(dir.path())
        .output()
        .await
        .expect("the binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--config",
        "--data-dir",
        "--addr",
        "--port",
        "--internal-addr",
        "--public-origin",
        "--team",
        "--backend",
        "--root",
        "--no-ticker",
        "--offline",
        "--print-config",
        "--check",
    ] {
        assert!(stdout.contains(flag), "--help is missing {flag}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_boots_prints_the_login_url_and_stops_cleanly() {
    let dir = tempfile::tempdir().expect("a temp dir");
    // A port that is free right now. There is a small race with another test
    // binding the same one; the retry below makes a collision a clear failure
    // rather than a hang.
    let port = free_port().await;

    let mut child = brain_command(dir.path())
        .args([
            // Nothing in this test may reach a model provider.
            "--offline",
            "--addr",
            &format!("127.0.0.1:{port}"),
            // The internal listener binds an ephemeral port; its address is in
            // the boot log, and the assertions below read it back out of there.
            "--team",
            &team_file().to_string_lossy(),
        ])
        .spawn()
        .expect("the daemon starts");

    let stdout = child.stdout.take().expect("piped stdout");
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&lines);
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            collected
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(line);
        }
    });

    // The boot sequence prints the one-time login URL, with the token in it.
    let login_url = wait_for_line(&lines, "#token=", Duration::from_secs(60))
        .await
        .unwrap_or_else(|| {
            panic!(
                "no login URL was printed:\n{}",
                lines.lock().unwrap_or_else(|e| e.into_inner()).join("\n")
            )
        });
    let token = login_url
        .split_once("#token=")
        .expect("a token in the URL")
        .1
        .trim()
        .to_string();
    assert!(
        login_url.contains(&format!("127.0.0.1:{port}")),
        "{login_url}"
    );
    // The token is on disk, owner-only, before the listener accepted anything.
    let on_disk = std::fs::read_to_string(dir.path().join("owner-token")).expect("a token file");
    assert_eq!(on_disk.trim(), token);
    assert!(swarmbot_brain::config::is_owner_only(
        &dir.path().join("owner-token")
    ));

    // The boot sequence also logged the roster it provisioned.
    assert!(
        wait_for_line(&lines, "the roster is ready", Duration::from_secs(30))
            .await
            .is_some(),
        "the boot sequence did not report the roster"
    );

    // Both listeners are up, and the internal one serves only its own routes.
    let internal_line = wait_for_line(
        &lines,
        "the internal listener is up",
        Duration::from_secs(30),
    )
    .await
    .expect("the internal listener was bound");
    let internal_addr = internal_line
        .split("listener=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no address in `{internal_line}`"))
        .to_string();
    let internal = common::TestBrain::client_with("");
    let value = internal
        .get(format!("http://{internal_addr}/health"))
        .send()
        .await
        .expect("the internal listener answers");
    let value = common::body(value).await;
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["listener"], serde_json::json!("internal"));
    // The owner's API is not reachable through it: a second path to the API would
    // be a second place to get authorization wrong.
    let response = internal
        .get(format!("http://{internal_addr}/api/bots"))
        .send()
        .await
        .expect("the internal listener answers");
    assert_eq!(response.status().as_u16(), 404);

    // It answers health, with the printed token.
    let client = common::TestBrain::client_with(&token);
    let base = format!("http://127.0.0.1:{port}");
    let mut healthy = None;
    for _ in 0..200 {
        match client.get(format!("{base}/api/health")).send().await {
            Ok(response) if response.status().is_success() => {
                healthy = Some(common::body(response).await);
                break;
            }
            Ok(_) | Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let health = healthy.unwrap_or_else(|| panic!("the daemon never answered on {base}"));
    assert_eq!(health["ok"], serde_json::json!(true));
    assert_eq!(
        health["version"],
        serde_json::json!(env!("CARGO_PKG_VERSION"))
    );

    // A graceful stop through the API, and then a zero exit code: the same path a
    // supervisor's SIGTERM takes.
    let response = client
        .post(format!("{base}/api/shutdown"))
        .send()
        .await
        .expect("the shutdown is accepted");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(common::body(response).await["ok"], serde_json::json!(true));

    let status = tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .expect("the daemon exits")
        .expect("the daemon was not killed by a signal");
    assert!(status.success(), "a clean shutdown exits zero: {status}");
    reader.abort();

    // The last thing it logged is that it stopped.
    let logged = lines.lock().unwrap_or_else(|e| e.into_inner()).join("\n");
    assert!(logged.contains("swarm stopped"), "{logged}");
    assert!(
        logged.contains("swarmbot-brain stopped"),
        "the daemon reports its own stop: {logged}"
    );
    // A restart of the same data dir prints no second login URL.
    let (code, stdout) = run_once(&["--check"], dir.path()).await;
    assert_eq!(code, Some(0), "{stdout}");
    assert!(!stdout.contains("#token="), "{stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_port_that_is_taken_is_reported_and_fails_the_boot() {
    let dir = tempfile::tempdir().expect("a temp dir");
    // Hold the port, then ask the daemon to bind it.
    let held = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a socket");
    let addr = held.local_addr().expect("an address");

    let output = brain_command(dir.path())
        .args([
            "--offline",
            "--addr",
            &addr.to_string(),
            "--internal-addr",
            "",
        ])
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("the binary runs");
    assert_ne!(
        output.status.code(),
        Some(0),
        "a busy port must fail the boot"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&addr.to_string()), "{stderr}");
    assert!(
        stderr.contains("cannot bind") || stderr.contains("address in use"),
        "{stderr}"
    );
    drop(held);
}

/// Find a port nothing is listening on, right now.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a socket");
    let port = listener.local_addr().expect("an address").port();
    drop(listener);
    port
}

/// Wait until one of the child's lines contains `needle`.
async fn wait_for_line(
    lines: &Arc<Mutex<Vec<String>>>,
    needle: &str,
    budget: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if let Some(line) = lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|line| line.contains(needle))
            .cloned()
        {
            return Some(line);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

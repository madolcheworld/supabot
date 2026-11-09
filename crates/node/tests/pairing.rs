//! Pairing: `POST /node/pair` against a hand-written fake brain on
//! `127.0.0.1:0`, and what lands on disk afterwards.
//!
//! This is one of only two test files that bind a port — pairing is HTTP, and
//! there is no in-memory transport for it. Everything asserted here is what the
//! real brain has to accept (see `docs/node-protocol.md` §2.3).

mod common;

use common::FakeBrain;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use serde_json::json;
use swarmbot_contracts::ProtocolRange;
use tokio::net::TcpListener;

use swarmbot_node::config::NodePaths;
use swarmbot_node::{Identity, NODE_VERSION, NodeConfig, NodeError};

fn brain_public_key(key: &SigningKey) -> String {
    URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())
}

#[tokio::test]
async fn pairing_pins_the_brain_key_at_0600_and_reports_the_node_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let brain_key = SigningKey::from_bytes(&[4u8; 32]);
    let node_id = swarmbot_contracts::NodeId::new();
    let public_key = brain_public_key(&brain_key);
    let claimed = node_id.to_string();
    // The `brainOrigin` in the response has to be the origin the node dialed,
    // which is only knowable once the responder has bound its port.
    let fake = FakeBrain::start_with(
        200,
        std::sync::Arc::new(move |addr| {
            json!({
                "nodeId": claimed.clone(),
                "brainPublicKey": public_key.clone(),
                "brainOrigin": format!("http://{addr}"),
            })
            .to_string()
        }),
    )
    .await;
    let url = fake.url();
    let origin = url.clone();

    // `abcd efgh` and `ABCD-EFGH` are one code (§2.1): the node normalizes
    // before it sends, and look-alikes are mapped the way the brain maps them.
    let outcome = swarmbot_node::cli::pair_command(Some(dir.path()), &url, "abcd-efgh")
        .await
        .expect("pairs");
    assert_eq!(outcome.node_id, node_id.to_string());
    assert_eq!(outcome.identity_path, NodePaths::new(dir.path()).identity());

    // The request the brain has to accept.
    let requests = fake.received();
    assert_eq!(requests.len(), 1);
    let recorded = &requests[0];
    assert!(
        recorded
            .request_line()
            .starts_with("POST /node/pair HTTP/1.1"),
        "{:?}",
        recorded.request_line()
    );
    assert_eq!(
        recorded.header("content-type"),
        Some("application/json"),
        "{:?}",
        recorded.request_line()
    );
    let request = recorded.pair_request();
    assert_eq!(request.code, "ABCDEFGH", "normalized before sending");
    assert_eq!(
        request.public_key.len(),
        43,
        "base64url without padding of a raw 32-byte key"
    );
    assert!(!request.public_key.contains('='));
    assert!(!request.hostname.is_empty());
    assert_eq!(request.platform, std::env::consts::OS);
    assert_eq!(request.arch.as_deref(), Some(std::env::consts::ARCH));
    assert_eq!(request.node_version.as_deref(), Some(NODE_VERSION));
    assert_eq!(request.protocol, Some(ProtocolRange::current()));

    // What landed on disk.
    let paths = NodePaths::new(dir.path());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(paths.identity())
            .expect("the identity exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the key file must be 0600");
    }
    let loaded = Identity::load(&paths).expect("loads");
    assert_eq!(loaded.node_id(), &node_id);
    assert_eq!(loaded.brain_origin(), origin);
    assert_eq!(
        loaded.brain_public_key(),
        &brain_key.verifying_key(),
        "the brain key is pinned, not re-fetched"
    );
    assert_eq!(
        request.public_key,
        loaded.public_key_base64url(),
        "the key we sent is the key we kept"
    );
    // The workspace and the audit/spill directories exist, so the first `run`
    // and the first audit line have somewhere to go.
    let config = NodeConfig::load(dir.path()).expect("loads");
    assert!(config.workspace().is_dir(), "the workspace was created");
    assert!(paths.audit().is_dir());
    assert!(paths.spill().is_dir());
    // ...and the brain URL is remembered, so `run` needs no `--brain`.
    assert_eq!(config.brain_url.as_deref(), Some(url.as_str()));
    // The private key is not in anything we returned or printed.
    let rendered = format!("{:?}", outcome);
    assert!(
        !rendered.contains(&hex::encode(loaded.signing_key().to_bytes())),
        "{rendered}"
    );
}

#[tokio::test]
async fn a_rejected_code_carries_the_brains_reason_and_writes_no_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body = json!({
        "error": {"code": "gone", "message": "pairing code expired"}
    })
    .to_string();
    let fake = FakeBrain::start(410, body).await;

    let error = swarmbot_node::cli::pair_command(Some(dir.path()), &fake.url(), "ABCD-EFGH")
        .await
        .expect_err("the brain refused");
    match &error {
        NodeError::PairingRejected { status, reason } => {
            assert_eq!(*status, 410);
            assert_eq!(reason, "pairing code expired", "the brain's own words");
        }
        other => panic!("expected PairingRejected, got {other:?}"),
    }
    // The message an operator sees is the brain's reason.
    assert!(
        error.to_string().contains("pairing code expired"),
        "{error}"
    );
    assert!(
        !NodePaths::new(dir.path()).identity().exists(),
        "a refused pairing writes nothing: a half-paired identity would make \
         the next `run` fail with a signature error instead of `not paired`"
    );
}

#[tokio::test]
async fn an_already_used_code_is_reported_as_the_brain_sees_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body = json!({"error": {"message": "pairing code already used"}}).to_string();
    let fake = FakeBrain::start(409, body).await;
    let error = swarmbot_node::cli::pair_command(Some(dir.path()), &fake.url(), "ABCD-EFGH")
        .await
        .expect_err("refused");
    assert!(error.to_string().contains("already used"), "{error}");
    assert!(!NodePaths::new(dir.path()).identity().exists());
}

#[tokio::test]
async fn a_non_json_refusal_still_reaches_the_operator() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A proxy or an older brain may not use the documented envelope; the raw body
    // beats "unknown error".
    let fake = FakeBrain::start(502, "upstream brain unavailable".to_string()).await;
    let error = swarmbot_node::cli::pair_command(Some(dir.path()), &fake.url(), "ABCD-EFGH")
        .await
        .expect_err("refused");
    assert!(
        error.to_string().contains("upstream brain unavailable"),
        "{error}"
    );
    assert!(!NodePaths::new(dir.path()).identity().exists());
}

#[tokio::test]
async fn a_code_with_a_character_outside_the_alphabet_never_reaches_the_brain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeBrain::start(200, "{}".to_string()).await;
    // `U` is excluded from Crockford base32, so this cannot be a code at all.
    let error = swarmbot_node::cli::pair_command(Some(dir.path()), &fake.url(), "ABCD-EFGU")
        .await
        .expect_err("refused locally");
    assert!(matches!(error, NodeError::Config(_)), "{error:?}");
    assert!(
        fake.received().is_empty(),
        "a code we can normalize is sent; one we cannot is not"
    );
    assert!(!NodePaths::new(dir.path()).identity().exists());
}

#[tokio::test]
async fn an_unreachable_brain_is_reported_as_unreachable_not_as_a_refusal() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Bind and drop, so the port is free and nothing is listening on it.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);

    let error =
        swarmbot_node::cli::pair_command(Some(dir.path()), &format!("http://{addr}"), "ABCD-EFGH")
            .await
            .expect_err("nothing is listening");
    match &error {
        NodeError::Unreachable { url, reason } => {
            assert!(url.ends_with("/node/pair"), "{url}");
            assert!(!reason.is_empty());
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }
    assert!(!NodePaths::new(dir.path()).identity().exists());
}

#[tokio::test]
async fn a_body_that_is_not_a_pair_response_is_a_protocol_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 200 with a body that is not a `PairResponse`: pinning nothing would leave
    // a node that cannot authenticate, so this is an error, not a pairing.
    let fake = FakeBrain::start(200, json!({"ok": true}).to_string()).await;
    let error = swarmbot_node::cli::pair_command(Some(dir.path()), &fake.url(), "ABCD-EFGH")
        .await
        .expect_err("refused");
    assert!(matches!(error, NodeError::Protocol(_)), "{error:?}");
    assert!(!NodePaths::new(dir.path()).identity().exists());
}

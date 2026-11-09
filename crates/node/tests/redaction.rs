//! Redaction: the private key exists in exactly one place (`<data>/identity.json`,
//! mode 0600) and in one private field. Nothing that can be formatted, logged or
//! written to a side file may carry it.
//!
//! This is the test AGENTS.md asks for whenever a type could hold a credential:
//! it formats every public type that transitively holds the keypair and fails if
//! any spelling of the secret appears.

mod common;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::*;
use ed25519_dalek::SigningKey;
use std::sync::Arc;
use swarmbot_contracts::PolicyMap;
use swarmbot_node::handlers::{NodeHandler, NodeRuntime};
use swarmbot_node::state::NodeState;
use swarmbot_node::{Identity, NodeConfig, NodeError, NodePaths};

/// Every spelling a leaked 32-byte Ed25519 seed could take.
fn spellings(key: &SigningKey) -> Vec<String> {
    let bytes = key.to_bytes();
    vec![
        URL_SAFE_NO_PAD.encode(bytes),
        base64::engine::general_purpose::STANDARD.encode(bytes),
        hex::encode(bytes),
        format!("{bytes:?}"),
        format!("{bytes:X?}"),
    ]
    .into_iter()
    // A spelling of all-zero bytes would match unrelated text; these keys
    // are not zero, so every spelling is distinctive.
    .filter(|text| text.len() >= 16)
    .collect()
}

fn assert_clean(label: &str, rendered: &str, secrets: &[String]) {
    for secret in secrets {
        assert!(
            !rendered.contains(secret.as_str()),
            "{label} leaked key material:\n{rendered}"
        );
    }
    // The upgrade nonce is not a credential, but there is no reason for it to
    // reach a log either.
    assert!(
        !rendered.contains(NONCE),
        "{label} printed the connection nonce:\n{rendered}"
    );
}

#[test]
fn no_public_debug_impl_prints_the_keypair() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).expect("root");
    let node_key = SigningKey::from_bytes(&[9u8; 32]);
    let brain = SigningKey::from_bytes(&[4u8; 32]);
    let secrets = spellings(&node_key);
    assert!(!secrets.is_empty());

    let paths = NodePaths::new(dir.path());
    paths.ensure_dirs(&root).expect("directories");
    let identity = Arc::new(
        Identity::from_parts(
            swarmbot_contracts::NodeId::new(),
            node_key.clone(),
            brain.verifying_key(),
            BRAIN_ORIGIN,
        )
        .expect("identity")
        .also_saved(&paths),
    );
    let mut config = NodeConfig::new_in(dir.path());
    config.roots = vec![swarmbot_contracts::NodeRoot::read_write(
        "root",
        root.display().to_string(),
    )];
    let runtime = NodeRuntime::new(config.clone(), Arc::clone(&identity)).expect("runtime");
    let handler = NodeHandler::new(Arc::clone(&runtime), NONCE);
    let state = NodeState::starting(
        std::process::id(),
        identity.node_id().as_str(),
        &config.label,
        identity.brain_origin(),
        Some("ws://127.0.0.1:1/node".into()),
        &paths.data_dir,
        runtime.roots(),
        config.ceilings.clone(),
        config.shell_enabled,
        config.capabilities().as_slice().to_vec(),
    );

    // Every public type that can reach the keypair, formatted both ways.
    assert_clean("Identity", &format!("{identity:?}"), &secrets);
    assert_clean("NodeRuntime", &format!("{runtime:?}"), &secrets);
    assert_clean("NodeHandler", &format!("{handler:?}"), &secrets);
    assert_clean("NodeConfig", &format!("{config:?}"), &secrets);
    assert_clean("NodeState", &format!("{state:?}"), &secrets);
    assert_clean(
        "NodeState (json)",
        &serde_json::to_string_pretty(&state).expect("serializes"),
        &secrets,
    );
    assert_clean(
        "PolicyMap",
        &format!("{:?}", PolicyMap::default()),
        &secrets,
    );
    for error in [
        NodeError::identity("boom"),
        NodeError::auth("boom"),
        NodeError::config("boom"),
        NodeError::Unreachable {
            url: "ws://x".into(),
            reason: "boom".into(),
        },
        NodeError::PairingRejected {
            status: 400,
            reason: "boom".into(),
        },
        NodeError::NotPaired {
            path: paths.identity(),
        },
    ] {
        assert_clean("NodeError", &format!("{error}"), &secrets);
        assert_clean("NodeError (debug)", &format!("{error:?}"), &secrets);
    }

    // The identity does report something useful instead: fingerprints of the
    // *public* halves, which is enough to tell two nodes apart in a log.
    let rendered = format!("{identity:?}");
    assert!(rendered.contains(&identity.fingerprint()), "{rendered}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(rendered.contains(identity.node_id().as_str()), "{rendered}");
}

trait AlsoSaved: Sized {
    fn also_saved(self, paths: &NodePaths) -> Self;
}

impl AlsoSaved for Identity {
    fn also_saved(self, paths: &NodePaths) -> Self {
        self.save(paths).expect("saves");
        self
    }
}

#[test]
fn the_only_copy_of_the_secret_on_disk_is_the_0600_identity_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).expect("root");
    let node_key = SigningKey::from_bytes(&[9u8; 32]);
    let secrets = spellings(&node_key);

    let paths = NodePaths::new(dir.path());
    paths.ensure_dirs(&root).expect("directories");
    let identity = Identity::from_parts(
        swarmbot_contracts::NodeId::new(),
        node_key,
        SigningKey::from_bytes(&[4u8; 32]).verifying_key(),
        BRAIN_ORIGIN,
    )
    .expect("identity");
    identity.save(&paths).expect("saves");

    let mut config = NodeConfig::new_in(dir.path());
    config.roots = vec![swarmbot_contracts::NodeRoot::read_write(
        "root",
        root.display().to_string(),
    )];
    let runtime = NodeRuntime::new(config, Arc::new(identity)).expect("runtime");
    runtime
        .audit
        .record(swarmbot_tools::AuditEntry::builder(&invocation(), "shell").build());
    let _ = swarmbot_node::state::StateWriter::start(
        &paths,
        &NodeState::starting(
            std::process::id(),
            runtime.identity.node_id().as_str(),
            &runtime.config.label,
            runtime.identity.brain_origin(),
            None,
            &paths.data_dir,
            runtime.roots(),
            runtime.config.ceilings.clone(),
            runtime.config.shell_enabled,
            runtime.config.capabilities().as_slice().to_vec(),
        ),
    )
    .expect("state file");
    runtime.config.save().expect("config");

    // Walk every file the daemon wrote and look for the secret. Exactly one file
    // may contain it, and that file must be 0600.
    let mut holders = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    walk(dir.path(), &mut holders);
    assert!(!holders.is_empty(), "the daemon wrote something");

    let mut leaked = Vec::new();
    for path in &holders {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        if secrets.iter().any(|secret| content.contains(secret)) {
            leaked.push(path.clone());
        }
    }
    assert_eq!(
        leaked,
        vec![paths.identity()],
        "only identity.json may hold the secret; found it in {leaked:?}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for path in &holders {
            let mode = std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            if path == &paths.identity() {
                assert_eq!(mode, 0o600, "{} must be 0600", path.display());
            } else {
                // Everything else is non-secret, but nothing may be world-writable.
                assert_eq!(mode & 0o002, 0, "{} is world-writable", path.display());
            }
        }
    }
}

#[test]
fn the_upgrade_header_carries_a_signature_and_nothing_that_looks_like_the_seed() {
    let node_key = SigningKey::from_bytes(&[9u8; 32]);
    let secrets = spellings(&node_key);
    let identity = Identity::from_parts(
        swarmbot_contracts::NodeId::new(),
        node_key,
        SigningKey::from_bytes(&[4u8; 32]).verifying_key(),
        BRAIN_ORIGIN,
    )
    .expect("identity");

    let (header, nonce) = identity.upgrade().expect("header");
    assert!(header.starts_with("SwarmBot-Node v1 id="), "{header}");
    assert_clean("the upgrade header", &header, &secrets);
    assert_clean("the nonce", &nonce, &secrets);
    // The public key is not in the header either: the brain already has it, and
    // a header that carried it would invite a substitution attack.
    assert!(
        !header.contains(&identity.public_key_base64url()),
        "{header}"
    );
    assert!(header.contains(&format!("nonce={nonce}")), "{header}");
}

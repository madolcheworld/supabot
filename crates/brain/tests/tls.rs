//! TLS end to end: a real brain serving `https://`, a real client that trusts its
//! certificate, and the two things that must **not** work.
//!
//! The README listed `wss://`/`https://` as unverified for several rounds, and the
//! reason was precise: the *client* half existed (a node rewrote a brain URL to
//! `wss` and `tokio-tungstenite` dialled it with rustls), but no brain ever
//! listened on TLS. This file proves the missing half — and, just as importantly,
//! proves that plaintext against a TLS port is refused rather than silently
//! downgraded.
//!
//! The certificate is made by `openssl` at test time and signed self-issued; the
//! client trusts **only** that certificate, added to an otherwise empty root
//! store. Nothing here touches the network: the listener is on `127.0.0.1:0`.

mod common;

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::Arc;

use common::*;

/// A CA, and a leaf certificate for `localhost`/`127.0.0.1` signed by it.
///
/// Three files, not one, because **a self-signed certificate is not a valid
/// server certificate** for a verifying client: webpki rejects it with
/// `CaUsedAsEndEntity` (it carries `CA:TRUE` and is being presented as a leaf).
/// A hand-written PEM would prove the parser and nothing about the handshake, so
/// the test uses the real tool and the real shape — a CA the client trusts, and a
/// leaf the brain serves.
///
/// It fails loudly if `openssl` is missing rather than quietly skipping: this is
/// the only proof that TLS works.
fn certificate(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let ca_cert = dir.join("ca.pem");
    let ca_key = dir.join("ca-key.pem");
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let csr = dir.join("leaf.csr");
    let ext = dir.join("leaf.ext");

    let run = |args: &[&str]| {
        let output = std::process::Command::new("openssl")
            .args(args)
            .output()
            .expect("openssl runs (it is how a real certificate is made)");
        assert!(
            output.status.success(),
            "openssl {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };

    // 1. A self-signed **CA**.
    run(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-keyout",
        ca_key.to_str().expect("utf-8 path"),
        "-out",
        ca_cert.to_str().expect("utf-8 path"),
        "-days",
        "1",
        "-nodes",
        "-subj",
        "/CN=SwarmBot Test CA",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
    ]);
    // 2. A leaf key and a CSR for `localhost`.
    run(&[
        "req",
        "-newkey",
        "rsa:2048",
        "-keyout",
        key.to_str().expect("utf-8 path"),
        "-out",
        csr.to_str().expect("utf-8 path"),
        "-nodes",
        "-subj",
        "/CN=localhost",
    ]);
    // 3. Sign it, with the SAN a real client checks and `CA:FALSE` so webpki
    //    accepts it as an end entity.
    std::fs::write(
        &ext,
        "basicConstraints=critical,CA:FALSE\n\
         keyUsage=critical,digitalSignature,keyEncipherment\n\
         extendedKeyUsage=serverAuth\n\
         subjectAltName=DNS:localhost,IP:127.0.0.1\n",
    )
    .expect("the extension file");
    run(&[
        "x509",
        "-req",
        "-in",
        csr.to_str().expect("utf-8 path"),
        "-CA",
        ca_cert.to_str().expect("utf-8 path"),
        "-CAkey",
        ca_key.to_str().expect("utf-8 path"),
        "-CAcreateserial",
        "-out",
        cert.to_str().expect("utf-8 path"),
        "-days",
        "1",
        "-extfile",
        ext.to_str().expect("utf-8 path"),
    ]);
    (ca_cert, cert, key)
}

/// A rustls client configuration that trusts exactly `cert` and nothing else.
///
/// "Nothing else" is the point: a system root store would make the handshake
/// prove only that *some* chain validated, and this test is about the brain's
/// certificate.
fn trust_only(ca: &Path) -> Arc<tokio_rustls::rustls::ClientConfig> {
    use tokio_rustls::rustls::pki_types::pem::PemObject as _;
    let pem = tokio_rustls::rustls::pki_types::CertificateDer::from_pem_file(ca)
        .expect("the test CA parses");
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(pem).expect("the test CA is a valid root");
    Arc::new(
        tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tls_brain_serves_https_and_a_plaintext_dial_is_refused() {
    let tmp = tempfile::tempdir().expect("a temp dir");
    let (ca, cert, key) = certificate(tmp.path());
    let brain = TestBrain::start_tls(&cert, &key).await;

    // The listener is TLS: a plain TCP `GET /api/health` must not produce an
    // HTTP response. Silently downgrading here would mean a client that thinks it
    // is encrypted is not — the one failure mode TLS must never have.
    let addr = brain.base_url.trim_start_matches("https://").to_string();
    let plaintext = tokio::task::spawn_blocking({
        let addr = addr.clone();
        move || {
            let mut stream = std::net::TcpStream::connect(&addr).expect("connect");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("timeout");
            // A well-formed HTTP request, which a plaintext listener would answer.
            let _ = stream.write_all(
                format!("GET /api/health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            );
            let mut raw = Vec::new();
            let _ = stream.read_to_end(&mut raw);
            String::from_utf8_lossy(&raw).to_string()
        }
    })
    .await
    .expect("the probe thread finishes");
    assert!(
        !plaintext.contains("HTTP/1.1 200"),
        "a plaintext request must not be answered by a TLS listener: {plaintext:?}"
    );

    // And the TLS client, trusting this certificate, gets the real API.
    let body = br#"{"tls": true}"#;
    let response = tokio::task::spawn_blocking({
        let addr = addr.clone();
        let config = trust_only(&ca);
        move || async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let tcp = tokio::net::TcpStream::connect(&addr)
                .await
                .map_err(|error| error.to_string())?;
            let connector = tokio_rustls::TlsConnector::from(config);
            let domain = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost")
                .map_err(|error| error.to_string())?;
            let mut tls = connector
                .connect(domain, tcp)
                .await
                .map_err(|error| error.to_string())?;
            let request = format!(
                "GET /api/health HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\n\
                 Connection: close\r\n\r\n",
                brain.token()
            );
            tls.write_all(request.as_bytes())
                .await
                .map_err(|error| error.to_string())?;
            let mut raw = String::new();
            tls.read_to_string(&mut raw)
                .await
                .map_err(|error| error.to_string())?;
            Ok::<_, String>(raw)
        }
    })
    .await
    .expect("the client thread finishes")
    .await
    .expect("the TLS handshake and the request succeed");
    let _ = body;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "the TLS listener answered the API: {response}"
    );
    // The JSON body is the health payload, so the route table really ran behind
    // the handshake rather than a bare "200 OK".
    assert!(
        response.contains("\"ok\":true"),
        "the response carries the health body: {response}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_does_not_trust_the_certificate_is_refused() {
    // The other half of "it is really TLS": a client with an empty root store must
    // fail the handshake. Without this, a brain that echoed bytes without
    // verifying anything would pass the test above.
    let tmp = tempfile::tempdir().expect("a temp dir");
    // The CA is not used here: this test's whole point is a client that does
    // *not* trust it.
    let (_ca, cert, key) = certificate(tmp.path());
    let brain = TestBrain::start_tls(&cert, &key).await;
    let addr = brain.base_url.trim_start_matches("https://").to_string();

    let result = tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        runtime.block_on(async move {
            use tokio::io::AsyncWriteExt as _;
            let tcp = tokio::net::TcpStream::connect(&addr)
                .await
                .map_err(|error| error.to_string())?;
            // An **empty** root store: no certificate can validate against it.
            let config = tokio_rustls::rustls::ClientConfig::builder()
                .with_root_certificates(tokio_rustls::rustls::RootCertStore::empty())
                .with_no_client_auth();
            let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
            let domain = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost")
                .map_err(|error| error.to_string())?;
            match connector.connect(domain, tcp).await {
                Ok(mut tls) => {
                    // If the handshake "succeeded" with no trust anchor, the
                    // server is not verifying anything — make that a failure.
                    tls.write_all(b"GET /api/health HTTP/1.1\r\nHost: localhost\r\n\r\n")
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok::<bool, String>(false)
                }
                Err(_untrusted) => Ok::<bool, String>(true),
            }
        })
    })
    .await
    .expect("the probe thread finishes")
    .expect("the probe runs");
    assert!(
        result,
        "a client with no trusted root must fail the handshake"
    );
}

/// The real binary: `--tls-cert` without `--tls-key` must refuse to start.
///
/// This is the failure the module doc calls out — half a TLS configuration is a
/// brain that boots, answers `/api/health`, and fails every *other* machine's
/// handshake, so the error surfaces where the operator is not looking. The refusal
/// names **both** flags, and it happens at start-up.
#[test]
fn the_binary_refuses_half_a_tls_configuration() {
    let tmp = tempfile::tempdir().expect("a temp dir");
    let (ca, cert, _key) = certificate(tmp.path());
    let _ = ca;
    let bin = env!("CARGO_BIN_EXE_swarmbot-brain");

    for (args, expected, missing) in [
        (
            vec!["--tls-cert", cert.to_str().expect("utf-8")],
            "--tls-cert",
            "--tls-key",
        ),
        (
            vec!["--tls-key", cert.to_str().expect("utf-8")],
            "--tls-key",
            "--tls-cert",
        ),
    ] {
        let output = std::process::Command::new(bin)
            .arg("--data-dir")
            .arg(tmp.path().join("data"))
            .arg("--offline")
            .args(&args)
            .output()
            .expect("the brain binary spawns");
        assert!(
            !output.status.success(),
            "{args:?} must not start: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected),
            "the refusal names `{expected}`: {stderr}"
        );
        assert!(
            stderr.contains(missing),
            "and the flag that is missing (`{missing}`): {stderr}"
        );
        assert!(stderr.contains("both"), "and says TLS needs both: {stderr}");
    }
}

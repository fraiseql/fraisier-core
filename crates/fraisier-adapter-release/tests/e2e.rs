//! End-to-end test: drive the **built** `fraisier-adapter-release` binary over the
//! real `IpcArtifactAdapter` (Local launcher), exercising the full JSON-RPC wire
//! path against a tiny in-test HTTP origin and a temp filesystem.
//!
//! Skips with a diagnostic (rather than failing) when the binary isn't built, so
//! `cargo test -p fraisier-adapter-release` is self-contained but the suite stays
//! green elsewhere.

#![cfg(unix)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;

use fraisier_core::adapter_axes::{AdapterCtx, ArtifactAdapter, HostId};
use fraisier_ipc::IpcArtifactAdapter;
use serde_json::json;

/// Locate the built binary (Cargo sets `CARGO_BIN_EXE_<name>` for the crate's own
/// bins when running its tests).
fn adapter_bin() -> Option<PathBuf> {
    option_env!("CARGO_BIN_EXE_fraisier-adapter-release")
        .map(PathBuf::from)
        .filter(|p| p.exists())
}

/// The bytes the test "release" contains.
const ARTIFACT: &[u8] = b"fraisier-release-e2e-payload";

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

/// Serve `body` over HTTP/1.1 for any GET, on an ephemeral loopback port. The
/// background thread loops (so retries are served) and is abandoned at test exit.
fn serve(body: Vec<u8>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0_u8; 1024];
            let _ = s.read(&mut buf); // drain the request line/headers
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(header.as_bytes());
            let _ = s.write_all(&body);
        }
    });
    addr
}

#[tokio::test]
async fn release_binary_stages_activates_and_reports_current_over_ipc() {
    let Some(bin) = adapter_bin() else {
        eprintln!("skipping: fraisier-adapter-release binary not built");
        return;
    };

    let origin = serve(ARTIFACT.to_vec());
    let root = tempfile::tempdir().expect("root");
    let staging = root.path().join("releases");
    let active = root.path().join("current");

    let adapter = IpcArtifactAdapter::new(&bin, "release");
    let host = HostId::new("web-1");

    let mut ctx = AdapterCtx::new("app", "production");
    ctx.settings.insert("version".into(), json!("1.2.3"));
    ctx.settings.insert(
        "release_url".into(),
        json!(format!("http://{origin}/app-{{version}}.tar.gz")),
    );
    ctx.settings
        .insert("sha256".into(), json!(sha256_hex(ARTIFACT)));
    ctx.settings
        .insert("staging_dir".into(), json!(staging.to_string_lossy()));
    ctx.settings
        .insert("active_path".into(), json!(active.to_string_lossy()));

    // stage: the host-side adapter downloads + verifies + writes the artifact.
    let staged = adapter.stage(&ctx, &host).await.expect("stage over ipc");
    assert_eq!(staged.artifact.id, "1.2.3");
    assert_eq!(
        std::fs::read(&staged.path).expect("staged bytes"),
        ARTIFACT,
        "the real release bytes were written on the host side"
    );

    // activate: the atomic symlink swap.
    adapter
        .activate(&ctx, &host, &staged)
        .await
        .expect("activate over ipc");
    assert_eq!(
        std::fs::read_link(&active).expect("current symlink"),
        staged.path
    );

    // current: report the active artifact id.
    let current = adapter
        .current(&ctx, &host)
        .await
        .expect("current over ipc");
    assert_eq!(current.expect("some").id, "1.2.3");
}

#[tokio::test]
async fn release_binary_rejects_a_checksum_mismatch_over_ipc() {
    let Some(bin) = adapter_bin() else {
        eprintln!("skipping: fraisier-adapter-release binary not built");
        return;
    };

    let origin = serve(ARTIFACT.to_vec());
    let root = tempfile::tempdir().expect("root");
    let adapter = IpcArtifactAdapter::new(&bin, "release");

    let mut ctx = AdapterCtx::new("app", "production");
    ctx.settings.insert("version".into(), json!("9.9.9"));
    ctx.settings.insert(
        "release_url".into(),
        json!(format!("http://{origin}/app-{{version}}.tar.gz")),
    );
    ctx.settings.insert(
        "sha256".into(),
        json!("0000000000000000000000000000000000000000000000000000000000000000"),
    );
    ctx.settings.insert(
        "staging_dir".into(),
        json!(root.path().join("rel").to_string_lossy()),
    );

    let err = adapter
        .stage(&ctx, &HostId::new("web-1"))
        .await
        .expect_err("a bad checksum must abort staging, surfaced over the wire");
    assert!(err.message.contains("checksum mismatch"), "got: {err}");
}

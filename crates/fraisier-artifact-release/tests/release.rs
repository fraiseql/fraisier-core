//! Integration tests for the release-artifact adapter.
//!
//! A tiny hand-rolled TCP server serves the artifact bytes (and, on a
//! `*.sha256` path, the checksum) so the tests need no network or fixture host.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use fraisier_artifact_release::ReleaseArtifact;
use fraisier_core::adapter_axes::{AdapterCtx, AdapterErrorKind, ArtifactAdapter, HostId};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

const ARTIFACT: &[u8] = b"fake-release-artifact-bytes-v1\n";

fn artifact_sha256() -> String {
    use std::fmt::Write as _;
    Sha256::digest(ARTIFACT)
        .iter()
        .fold(String::new(), |mut acc, byte| {
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

/// Serve `ARTIFACT` on any path, and its checksum hex on a `*.sha256` path.
async fn start_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 2048];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_owned();

            let body: Vec<u8> = if path.ends_with(".sha256") {
                format!("{}  app.tar.gz\n", artifact_sha256()).into_bytes()
            } else {
                ARTIFACT.to_vec()
            };
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(header.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    });
    addr
}

fn base_ctx(addr: SocketAddr, staging: &Path) -> AdapterCtx {
    let mut ctx = AdapterCtx::new("checkout", "production");
    ctx.settings.insert("version".to_owned(), json!("1"));
    ctx.settings.insert(
        "release_url".to_owned(),
        json!(format!("http://{addr}/app-{{version}}.tar.gz")),
    );
    ctx.settings
        .insert("staging_dir".to_owned(), json!(staging.to_str().unwrap()));
    ctx.settings.insert("attempts".to_owned(), json!(1));
    ctx.settings.insert("retry_delay_ms".to_owned(), json!(0));
    ctx.settings.insert("timeout_ms".to_owned(), json!(2000));
    ctx
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fraisier-rel-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn host() -> HostId {
    HostId::new("web-1")
}

#[tokio::test]
async fn stage_downloads_and_verifies_inline_sha256() {
    let addr = start_server().await;
    let dir = tempdir("inline");
    let mut ctx = base_ctx(addr, &dir);
    ctx.settings
        .insert("sha256".to_owned(), json!(artifact_sha256()));

    let staged = ReleaseArtifact::new()
        .stage(&ctx, &host())
        .await
        .expect("stage");
    assert_eq!(staged.artifact.id, "1");
    assert_eq!(
        staged.artifact.checksum.as_deref(),
        Some(artifact_sha256().as_str())
    );
    assert_eq!(std::fs::read(&staged.path).expect("read staged"), ARTIFACT);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stage_rejects_checksum_mismatch() {
    let addr = start_server().await;
    let dir = tempdir("mismatch");
    let mut ctx = base_ctx(addr, &dir);
    ctx.settings
        .insert("sha256".to_owned(), json!("00".repeat(32)));

    let err = ReleaseArtifact::new()
        .stage(&ctx, &host())
        .await
        .expect_err("a corrupted download must not be staged");
    assert_eq!(err.kind, AdapterErrorKind::Execution);
    assert!(err.to_string().contains("checksum mismatch"));
    // Nothing should have been written.
    assert!(!dir.join("1").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stage_fetches_checksum_url() {
    let addr = start_server().await;
    let dir = tempdir("csurl");
    let mut ctx = base_ctx(addr, &dir);
    ctx.settings.insert(
        "checksum_url".to_owned(),
        json!(format!("http://{addr}/app-{{version}}.tar.gz.sha256")),
    );

    let staged = ReleaseArtifact::new()
        .stage(&ctx, &host())
        .await
        .expect("stage");
    assert_eq!(
        staged.artifact.checksum.as_deref(),
        Some(artifact_sha256().as_str())
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn activate_then_current_roundtrip() {
    let addr = start_server().await;
    let dir = tempdir("roundtrip");
    let active = dir.join("current");
    let mut ctx = base_ctx(addr, &dir);
    ctx.settings
        .insert("sha256".to_owned(), json!(artifact_sha256()));
    ctx.settings
        .insert("active_path".to_owned(), json!(active.to_str().unwrap()));
    let adapter = ReleaseArtifact::new();

    // Before activation, there is no current artifact.
    assert_eq!(adapter.current(&ctx, &host()).await.expect("current"), None);

    let staged = adapter.stage(&ctx, &host()).await.expect("stage");
    adapter
        .activate(&ctx, &host(), &staged)
        .await
        .expect("activate");

    let current = adapter
        .current(&ctx, &host())
        .await
        .expect("current")
        .expect("an artifact is active");
    assert_eq!(current.id, "1");
    assert_eq!(std::fs::read(&active).expect("read via symlink"), ARTIFACT);

    let _ = std::fs::remove_dir_all(&dir);
}

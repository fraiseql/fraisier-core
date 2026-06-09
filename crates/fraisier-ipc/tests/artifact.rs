//! Integration tests for the IPC **artifact** adapter against a shell fixture that
//! speaks the `Content-Length`-framed JSON-RPC adapter protocol — driven both
//! locally and through a fake `ssh` (proving the IPC-over-SSH launch path).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use fraisier_core::adapter_axes::{AdapterCtx, ArtifactAdapter, HostId};
use fraisier_ipc::{IpcArtifactAdapter, Launcher, SshLauncher};
use serde_json::json;

/// A POSIX-shell `fraisier-adapter-release` stand-in: drains the framed request,
/// reads which method was asked for, and emits the matching canned framed
/// response. JSON is serialized compactly, so `"method":"stage"` appears verbatim.
const FAKE_ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
emit() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
case "$req" in
  *'"method":"stage"'*)    emit '{"jsonrpc":"2.0","id":1,"result":{"artifact":{"id":"1.2.3","checksum":"deadbeef"},"path":"/srv/app/releases/1.2.3"}}' ;;
  *'"method":"activate"'*) emit '{"jsonrpc":"2.0","id":1,"result":null}' ;;
  *'"method":"current"'*)  emit '{"jsonrpc":"2.0","id":1,"result":{"id":"1.2.3","checksum":null}}' ;;
  *)                       emit '{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"unknown method"}}' ;;
esac
"#;

/// A crude fake `ssh`: skip `-o/-p/-i` option pairs and other flags, consume the
/// destination, then exec whatever remote command follows — so the framed
/// request/response flows through stdio exactly as it would over a real ssh pipe.
const FAKE_SSH: &str = r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    -o|-p|-i) shift 2 ;;
    -*) shift ;;
    *) shift; break ;;
  esac
done
exec sh -c "$*"
"#;

/// Write `body` to a fresh executable file under a kept-alive temp dir.
fn write_exe(body: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("exe");
    std::fs::write(&path, body).expect("write");
    let mut perms = std::fs::metadata(&path).expect("meta").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    (dir, path)
}

fn ctx_for(address: &str) -> AdapterCtx {
    let mut ctx = AdapterCtx::new("app", "production");
    ctx.settings.insert("address".to_owned(), json!(address));
    ctx.settings.insert("version".to_owned(), json!("1.2.3"));
    ctx
}

async fn assert_roundtrip(adapter: &IpcArtifactAdapter, ctx: &AdapterCtx, where_: &str) {
    let host = HostId::new("web-1");

    let staged = adapter.stage(ctx, &host).await.expect("stage round-trips");
    assert_eq!(staged.artifact.id, "1.2.3", "{where_}");
    assert_eq!(
        staged.artifact.checksum.as_deref(),
        Some("deadbeef"),
        "{where_}"
    );
    assert_eq!(
        staged.path,
        Path::new("/srv/app/releases/1.2.3"),
        "{where_}"
    );

    adapter
        .activate(ctx, &host, &staged)
        .await
        .expect("activate decodes a null result as unit");

    let current = adapter.current(ctx, &host).await.expect("current");
    assert_eq!(current.expect("some").id, "1.2.3", "{where_}");
}

#[tokio::test]
async fn artifact_round_trips_over_the_local_launcher() {
    let (_dir, adapter_bin) = write_exe(FAKE_ADAPTER);
    let adapter = IpcArtifactAdapter::new(&adapter_bin, "release");
    // Local launcher ignores the address; provide one anyway to mirror real ctx.
    assert_roundtrip(&adapter, &ctx_for("ignored.local"), "local").await;
}

#[tokio::test]
async fn artifact_round_trips_over_the_ssh_launcher() {
    let (_adir, adapter_bin) = write_exe(FAKE_ADAPTER);
    let (_sdir, fake_ssh) = write_exe(FAKE_SSH);

    let adapter = IpcArtifactAdapter::new(&adapter_bin, "release").with_launcher(Launcher::ssh(
        SshLauncher::new()
            .with_user("deploy")
            .with_program(&fake_ssh),
    ));
    // The Ssh launcher resolves the host from the ctx address and runs the adapter
    // "remotely" (here: the fake ssh execs it locally), proving the launch path.
    assert_roundtrip(&adapter, &ctx_for("web1.internal"), "ssh").await;
}

#[tokio::test]
async fn ssh_launcher_without_an_address_errors() {
    let (_adir, adapter_bin) = write_exe(FAKE_ADAPTER);
    let (_sdir, fake_ssh) = write_exe(FAKE_SSH);

    let adapter = IpcArtifactAdapter::new(&adapter_bin, "release")
        .with_launcher(Launcher::ssh(SshLauncher::new().with_program(&fake_ssh)));

    let err = adapter
        .stage(&AdapterCtx::new("app", "production"), &HostId::new("web-1"))
        .await
        .expect_err("no address must error before spawning ssh");
    assert!(
        err.message.contains("needs a target host address"),
        "got: {}",
        err.message
    );
}

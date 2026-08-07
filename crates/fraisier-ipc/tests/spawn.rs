//! The IPC spawn path under `ETXTBSY` ("text file busy").
//!
//! Linux refuses to `exec` a file that any process still holds open for writing.
//! It is the one spawn failure that is *always* transient — the writer will close
//! — so the client rides it out with a short bounded retry rather than failing a
//! deploy on a race. These tests pin both halves of that contract: a binary whose
//! writer closes mid-window still round-trips, and one held busy throughout still
//! surfaces the error instead of hanging.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fraisier_core::adapter_axes::{AdapterErrorKind, MigrationAdapter as _};
use fraisier_ipc::IpcMigrationAdapter;

/// A POSIX-shell adapter fixture: drain the framed request, emit one framed
/// `describe` response.
const FAKE_ADAPTER: &str = r#"#!/bin/sh
cat >/dev/null
body='{"jsonrpc":"2.0","id":1,"result":{"name":"fixture","version":"0.0.1","protocol_version":1,"capabilities":["describe"]}}'
printf 'Content-Length: %d\r\n\r\n%s' "${#body}" "$body"
"#;

/// How long the writer holds its fd open in the transient case. Comfortably
/// longer than the first spawn attempt, comfortably shorter than the retry
/// budget, so the test is decisive in both directions.
const HOLD: Duration = Duration::from_millis(50);

/// Write [`FAKE_ADAPTER`] as a fresh executable under a kept-alive temp dir.
fn write_adapter() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fraisier-adapter-fixture");
    std::fs::write(&path, FAKE_ADAPTER).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    (dir, path)
}

/// Open a *writer* fd on `path` — the exact condition Linux raises `ETXTBSY`
/// for. Opening happens on the caller's thread, so the file is provably busy
/// before the caller spawns; the returned handle closes it after `hold`.
fn hold_writer_for(path: &Path, hold: Duration) -> std::thread::JoinHandle<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the adapter for writing");
    std::thread::spawn(move || {
        std::thread::sleep(hold);
        drop(file);
    })
}

#[tokio::test]
async fn a_transiently_busy_adapter_still_round_trips() {
    let (_dir, adapter_bin) = write_adapter();
    let writer = hold_writer_for(&adapter_bin, HOLD);

    let description = IpcMigrationAdapter::new(&adapter_bin, "fixture")
        .describe()
        .await
        .expect("a spawn racing a closing writer must be retried, not failed");
    assert_eq!(description.name, "fixture");

    writer.join().expect("writer thread");
}

#[tokio::test]
async fn an_adapter_held_busy_throughout_surfaces_the_spawn_error() {
    let (_dir, adapter_bin) = write_adapter();
    // Held for the whole call: the retry must give up and report, never hang and
    // never dress `ETXTBSY` up as something else.
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&adapter_bin)
        .expect("open the adapter for writing");

    let err = IpcMigrationAdapter::new(&adapter_bin, "fixture")
        .describe()
        .await
        .expect_err("a permanently busy adapter must fail, not spin");
    assert_eq!(err.kind, AdapterErrorKind::Execution);
    assert!(
        err.message.contains("failed to spawn adapter"),
        "got: {}",
        err.message
    );

    drop(writer);
}

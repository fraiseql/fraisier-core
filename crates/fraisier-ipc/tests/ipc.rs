//! Integration tests for the IPC migration adapter against a shell fixture that
//! speaks the `Content-Length`-framed JSON-RPC adapter protocol.

#![cfg(unix)]

use std::time::Duration;

use fraisier_core::adapter_axes::{AdapterCtx, AdapterErrorKind, MigrationAdapter};
use fraisier_ipc::IpcMigrationAdapter;

/// A POSIX-shell adapter fixture: it drains the framed request from stdin, then
/// emits a single framed JSON-RPC response read from the `FIXTURE_BODY` env var.
const FIXTURE_SCRIPT: &str =
    r#"cat >/dev/null; printf 'Content-Length: %d\r\n\r\n%s' "${#FIXTURE_BODY}" "$FIXTURE_BODY""#;

#[tokio::test]
async fn describe_round_trips_over_ipc() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"name":"fixture","version":"0.0.1","protocol_version":1,"capabilities":["describe","up","preflight"]}}"#;
    let adapter = IpcMigrationAdapter::new("sh", "fixture")
        .with_args(["-c", FIXTURE_SCRIPT])
        .with_env("FIXTURE_BODY", body);

    let desc = adapter.describe().await.expect("describe round-trips");
    assert_eq!(desc.name, "fixture");
    assert_eq!(desc.protocol_version, 1);
    assert!(desc.capabilities.iter().any(|c| c == "preflight"));
}

#[tokio::test]
async fn remote_error_is_mapped_to_adapter_error() {
    let body =
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32010,"message":"migrations dir not found"}}"#;
    let adapter = IpcMigrationAdapter::new("sh", "fixture")
        .with_args(["-c", FIXTURE_SCRIPT])
        .with_env("FIXTURE_BODY", body);

    let err = adapter
        .current_revision(&AdapterCtx::new("checkout", "production"))
        .await
        .expect_err("remote error surfaces as Err");
    assert_eq!(err.code, -32010, "the remote JSON-RPC code is preserved");
    assert!(err.message.contains("migrations dir not found"));
}

#[tokio::test]
async fn missing_adapter_is_reported_as_not_found_on_path() {
    let adapter = IpcMigrationAdapter::new("fraisier-adapter-surely-not-real-9999", "ghost");
    let err = adapter.describe().await.expect_err("spawn fails");
    assert_eq!(err.kind, AdapterErrorKind::Execution);
    assert!(
        err.message.contains("not found on PATH"),
        "got: {}",
        err.message
    );
}

#[tokio::test]
async fn crash_without_response_carries_exit_status_and_stderr() {
    // Drains the request, writes to stderr, exits non-zero — no framed response.
    let adapter = IpcMigrationAdapter::new("sh", "crasher")
        .with_args(["-c", "cat >/dev/null; echo boom >&2; exit 3"]);
    let err = adapter.describe().await.expect_err("crash surfaces as Err");
    assert!(
        err.message.contains("without sending a response") && err.message.contains("status 3"),
        "got: {}",
        err.message
    );
    assert_eq!(err.stderr.as_deref().map(str::trim), Some("boom"));
}

#[tokio::test]
async fn hung_adapter_is_killed_on_timeout() {
    // Never responds; the short timeout must kill it rather than block forever.
    let adapter = IpcMigrationAdapter::new("sh", "sleeper")
        .with_args(["-c", "sleep 30"])
        .with_timeout(Duration::from_millis(200));
    let err = adapter
        .describe()
        .await
        .expect_err("timeout surfaces as Err");
    assert!(
        err.message.contains("did not respond within"),
        "got: {}",
        err.message
    );
}

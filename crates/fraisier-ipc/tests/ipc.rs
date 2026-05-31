//! Integration tests for the IPC migration adapter against a shell fixture that
//! speaks the `Content-Length`-framed JSON-RPC adapter protocol.

#![cfg(unix)]

use fraisier_core::adapter_axes::{AdapterCtx, MigrationAdapter};
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

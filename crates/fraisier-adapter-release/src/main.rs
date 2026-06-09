//! # fraisier-adapter-release
//!
//! A first-party fraisier **artifact** IPC adapter. It speaks the
//! JSON-RPC-over-stdio adapter protocol (v1, see
//! `crates/fraisier-ipc/PROTOCOL.md`) and backs each call with the in-process
//! [`ReleaseArtifact`] adapter — download a release over HTTP, verify its
//! SHA-256, stage it, and activate it via an atomic symlink swap.
//!
//! It is the **host-side half of the IPC-over-SSH artifact path**: `fraisier`
//! launches it on each host (`ssh host -- fraisier-adapter-release`) and the
//! JSON-RPC flows through ssh's stdio, so the adapter does its filesystem/HTTP
//! work *locally on the host* — no `curl`/coreutils dependency, just this binary.
//! One spawn per call (the request arrives on stdin, the response leaves on
//! stdout, then the process exits), so a crash never outlives a single call. The
//! same binary also serves a local (`source = "release-ipc"` single-host) deploy.

use std::process::ExitCode;

use fraisier_artifact_release::ReleaseArtifact;
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterError, AdapterErrorKind, ArtifactAdapter, HostId, StagedArtifact,
};
use fraisier_ipc::server;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

/// The IPC protocol major version this adapter speaks.
const PROTOCOL_VERSION: u32 = 1;

/// The adapter's discovery name (`fraisier-adapter-<name>`).
const ADAPTER_NAME: &str = "release";

/// One framed request in, one framed response out, then exit.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Read + parse the one request before the match, so the stdin lock does not
    // live across the dispatch await.
    let request = server::read_request(&mut std::io::stdin().lock());
    let response = match request {
        Ok(request) => {
            let id = server::normalize_id(request.id);
            match dispatch(&request.method, request.params).await {
                Ok(result) => server::success(id, result),
                Err(error) => server::error_response(id, &error),
            }
        }
        // A framing/parse failure is already a framed JSON-RPC error envelope.
        Err(envelope) => envelope,
    };

    let body = match serde_json::to_vec(&response) {
        Ok(body) => body,
        Err(e) => {
            eprintln!("fraisier-adapter-release: failed to encode response: {e}");
            return ExitCode::FAILURE;
        }
    };
    // A logical failure travels in the JSON-RPC `error` field, not the exit code;
    // exit 0 lets the host read our framed error rather than treating us as a
    // crash-without-response.
    if let Err(e) = server::write_framed(&mut std::io::stdout().lock(), &body) {
        eprintln!("fraisier-adapter-release: failed to write response: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Dispatch one method to the in-process [`ReleaseArtifact`] adapter.
async fn dispatch(method: &str, params: Value) -> Result<Value, AdapterError> {
    let adapter = ReleaseArtifact::new();
    match method {
        "describe" => Ok(serde_json::json!({
            "name": ADAPTER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "protocol_version": PROTOCOL_VERSION,
            "capabilities": ["stage", "activate", "current"],
        })),
        "stage" => {
            let (ctx, host) = ctx_host(&params)?;
            encode(&adapter.stage(&ctx, &host).await?)
        }
        "activate" => {
            let (ctx, host) = ctx_host(&params)?;
            let staged: StagedArtifact = field(&params, "staged")?;
            adapter.activate(&ctx, &host, &staged).await?;
            Ok(Value::Null)
        }
        "current" => {
            let (ctx, host) = ctx_host(&params)?;
            encode(&adapter.current(&ctx, &host).await?)
        }
        other => Err(AdapterError::method_not_supported(other)),
    }
}

/// Deserialize the `ctx` + `host` every per-host artifact method carries.
fn ctx_host(params: &Value) -> Result<(AdapterCtx, HostId), AdapterError> {
    Ok((field(params, "ctx")?, field(params, "host")?))
}

/// Deserialize `params.<key>` into `T`, or an `InvalidConfig` error.
fn field<T: DeserializeOwned>(params: &Value, key: &str) -> Result<T, AdapterError> {
    let value = params.get(key).ok_or_else(|| {
        fail(
            AdapterErrorKind::InvalidConfig,
            format!("missing params.{key}"),
        )
    })?;
    serde_json::from_value(value.clone()).map_err(|e| {
        fail(
            AdapterErrorKind::InvalidConfig,
            format!("invalid params.{key}: {e}"),
        )
    })
}

/// Serialize a result value, mapping an encode failure to a protocol error.
fn encode<T: Serialize>(value: &T) -> Result<Value, AdapterError> {
    serde_json::to_value(value).map_err(|e| {
        fail(
            AdapterErrorKind::Protocol,
            format!("failed to encode result: {e}"),
        )
    })
}

/// Build an error tagged with this adapter.
fn fail(kind: AdapterErrorKind, message: String) -> AdapterError {
    AdapterError {
        adapter: Some(ADAPTER_NAME.to_owned()),
        ..AdapterError::new(kind, message)
    }
}

//! JSON-RPC 2.0 envelope types for the adapter protocol (see `PROTOCOL.md`).

use serde::Deserialize;

/// The IPC protocol major version this client speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// A JSON-RPC 2.0 response from an adapter: exactly one of `result`/`error` is set.
#[derive(Debug, Deserialize)]
pub struct Response {
    /// Echoes the request id, when present.
    #[serde(default)]
    pub id: Option<u64>,
    /// The success payload.
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    /// The failure payload.
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Deserialize)]
pub struct RpcError {
    /// The numeric error code (preserved into `AdapterError::code`).
    pub code: i32,
    /// The human-readable message.
    pub message: String,
    /// Optional structured data.
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

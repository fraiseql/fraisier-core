//! JSON-RPC 2.0 envelope types for the adapter protocol (see `PROTOCOL.md`).

use serde::{Deserialize, Deserializer};

/// The IPC protocol major version this client speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// A JSON-RPC 2.0 response from an adapter: exactly one of `result`/`error` is set.
#[derive(Debug, Deserialize)]
pub struct Response {
    /// Echoes the request id, when present.
    #[serde(default)]
    pub id: Option<u64>,
    /// The success payload. `None` means the `result` member was *absent*;
    /// `Some(Value::Null)` means it was present and JSON `null` (a legitimate
    /// result — e.g. `current_revision` with no migrations applied, or
    /// `post_migrate` returning unit). A plain `Option<Value>` cannot tell those
    /// apart (serde collapses a present `null` to `None`), so a present `null`
    /// would otherwise be misread as "no result".
    #[serde(default, deserialize_with = "present_value")]
    pub result: Option<serde_json::Value>,
    /// The failure payload.
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// Deserialize a field so a *present* value — including JSON `null` — becomes
/// `Some(..)`. Only invoked when the key is present; an absent key falls back to
/// the field's `#[serde(default)]` (`None`).
fn present_value<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
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

//! JSON-RPC-over-stdio IPC client for fraisier migration adapters.
//!
//! [`IpcMigrationAdapter`] spawns an external `fraisier-adapter-<name>` binary and
//! implements [`MigrationAdapter`](fraisier_core::adapter_axes::MigrationAdapter)
//! by serializing each trait call to a `Content-Length`-framed JSON-RPC request
//! on the child's stdin and reading the framed response from its stdout. Because
//! the trait's arguments and returns are all `Serialize + Deserialize` (the
//! convergence rule), an IPC adapter is interchangeable with an in-process one.
//!
//! The wire format is specified in `PROTOCOL.md` (the spec adapter authors
//! implement against, in any language); the `framing` and `protocol` modules
//! here are this Rust client's private implementation of it.

mod adapter;
mod framing;
mod protocol;

pub use adapter::IpcMigrationAdapter;

/// The IPC protocol major version this client speaks (checked via `describe`).
pub const PROTOCOL_VERSION: u32 = protocol::PROTOCOL_VERSION;

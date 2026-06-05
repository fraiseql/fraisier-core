//! JSON-RPC-over-stdio IPC client for fraisier adapters.
//!
//! An IPC adapter spawns an external `fraisier-adapter-<name>` binary and
//! implements one of the [`adapter_axes`](fraisier_core::adapter_axes) traits by
//! serializing each trait call to a `Content-Length`-framed JSON-RPC request on
//! the child's stdin and reading the framed response from its stdout. Because the
//! traits' arguments and returns are all `Serialize + Deserialize` (the
//! convergence rule), an IPC adapter is interchangeable with an in-process one.
//!
//! The axis-agnostic transport lives in the private `client` module (`IpcClient`);
//! each axis is a thin wrapper over it:
//!
//! - [`IpcMigrationAdapter`] — the migration axis (always run locally on the
//!   orchestrator: the DSN secret never leaves the box, Decision 5).
//! - [`IpcArtifactAdapter`] — the artifact axis, which a [`Launcher::Ssh`] can run
//!   **on the target host** so a rich in-process adapter does its filesystem/HTTP
//!   work where the files live (the IPC-over-SSH model).
//!
//! [`Launcher`] decides *where* the subprocess runs — locally, or on a remote host
//! over `ssh` with `ControlMaster` connection reuse. The wire format is specified
//! in `PROTOCOL.md` (the spec adapter authors implement against, in any language);
//! the `framing` and `protocol` modules here are this Rust client's private
//! implementation of it.

mod adapter;
mod artifact;
mod client;
mod framing;
mod launcher;
mod protocol;

pub use adapter::IpcMigrationAdapter;
pub use artifact::IpcArtifactAdapter;
pub use launcher::{Launcher, SshLauncher};

/// The IPC protocol major version this client speaks (checked via `describe`).
pub const PROTOCOL_VERSION: u32 = protocol::PROTOCOL_VERSION;

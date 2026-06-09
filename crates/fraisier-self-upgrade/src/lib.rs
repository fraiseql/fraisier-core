//! # fraisier-self-upgrade
//!
//! The engine behind `fraisier self-upgrade apply`. It upgrades fraisier's *own*
//! binary — the most dangerous swap fraisier makes, because the recovery tool is
//! inside the blast radius — and proves its **auto-revert** failure path under
//! test before it ships.
//!
//! ## The model (systemd-managed swap)
//!
//! The supervised unit's `ExecStart` points at a stable symlink
//! (`…/bin/current`). [`apply`](crate::apply) stages the new binary beside it,
//! verifies its SHA-256, **atomically repoints the symlink**, restarts the unit,
//! and health-checks it. On an unhealthy/timed-out start it **repoints the
//! symlink back at the kept-old target**, restarts again, and re-probes.
//!
//! ## Load-bearing invariant
//!
//! After the swap the controller **never `exec`s the swapped binary** — not for
//! the probe, the restart, or the revert. It drives [`Supervisor`] (`systemctl`)
//! and [`Health`] (an out-of-process HTTP `GET /healthz`). Both are out of
//! process, so the supervisor — not the possibly-broken new binary — performs the
//! restart, and the revert survives a binary that boots-then-dies. An in-process
//! re-exec model is rejected: re-exec into a boots-then-dies binary kills the very
//! process that would revert.

mod apply;
mod notify;
mod source;
mod swap;
mod system;

pub use apply::{apply, ApplyOutcome, Health, Plan, Supervisor};
pub use notify::{ExecHookNotifier, FailurePayload, NoopNotifier, Notifier, TracingNotifier};
pub use source::{Source, Verified};
pub use swap::Layout;
pub use system::{systemd_available, HttpHealth, SystemctlSupervisor};

/// A fetch/verify/IO failure that aborts an upgrade **before** any swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The binary could not be fetched (transport / HTTP / missing file).
    Fetch(String),
    /// A local filesystem operation failed.
    Io(String),
    /// The downloaded bytes did not match the configured checksum.
    ChecksumMismatch {
        /// The expected SHA-256 (hex, lower-case).
        expected: String,
        /// The SHA-256 actually computed over the bytes (hex, lower-case).
        actual: String,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fetch(message) => write!(f, "fetch failed: {message}"),
            Self::Io(message) => write!(f, "io error: {message}"),
            Self::ChecksumMismatch { expected, actual } => {
                write!(f, "checksum mismatch: expected {expected}, got {actual}")
            }
        }
    }
}

impl std::error::Error for Error {}

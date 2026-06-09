//! Declarative project checks: a list of shell commands fraisier runs with
//! cross-check parallelism, as a single source of truth runnable both locally
//! (`fraisier check`) and in CI, and used to gate `fraisier ship`.
//!
//! A [`Check`] is a named shell command. [`run`] executes a slice of them with a
//! bounded number running concurrently and returns a [`CheckRunReport`] whose
//! outcomes are in the original (config) order regardless of completion order.

use std::path::PathBuf;
use std::time::Duration;

mod runner;

pub use runner::run;

/// One check to run: a named shell command, optionally in a sub-directory.
///
/// `command` is a shell string run via `sh -c`, so intra-check parallelism (for
/// example `pytest -n auto`) lives inside the string; cross-check parallelism is
/// [`run`]'s `jobs` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// The check's display name, unique within a run.
    pub name: String,
    /// The shell command, run via `sh -c "<command>"`.
    pub command: String,
    /// The directory the command runs in, already resolved to an absolute path
    /// by the caller; `None` runs in [`run`]'s base directory.
    pub workdir: Option<PathBuf>,
}

/// Whether a [`Check`] passed, failed, or could not be spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// The command exited with status `0`.
    Passed,
    /// The command ran and exited non-zero (or was killed by a signal).
    Failed,
    /// The command could not be spawned at all (for example a missing shell).
    SpawnError,
}

/// The captured outcome of running one [`Check`].
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    /// The check's name, echoed for reporting in config order.
    pub name: String,
    /// Whether the check passed, failed, or failed to spawn.
    pub status: CheckStatus,
    /// The process exit code, or `None` when killed by a signal or not spawned.
    pub code: Option<i32>,
    /// Captured standard output (lossy UTF-8).
    pub stdout: String,
    /// Captured standard error (lossy UTF-8); the spawn error when
    /// [`CheckStatus::SpawnError`].
    pub stderr: String,
    /// How long the command took.
    pub duration: Duration,
}

impl CheckOutcome {
    /// Whether this check passed.
    #[must_use]
    pub const fn passed(&self) -> bool {
        matches!(self.status, CheckStatus::Passed)
    }
}

/// The aggregate result of a [`run`]: every [`CheckOutcome`] in config order,
/// regardless of the concurrent execution order.
#[derive(Debug, Clone)]
pub struct CheckRunReport {
    /// Per-check outcomes, in the order the checks were given.
    pub outcomes: Vec<CheckOutcome>,
    /// Total wall-clock time of the whole run (overlapping across concurrency).
    pub total_duration: Duration,
}

impl CheckRunReport {
    /// Whether every check passed.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.outcomes.iter().all(CheckOutcome::passed)
    }

    /// How many checks failed (counting spawn errors).
    #[must_use]
    pub fn failed_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| !outcome.passed())
            .count()
    }

    /// The names of the failed checks, in config order.
    pub fn failed_names(&self) -> impl Iterator<Item = &str> {
        self.outcomes
            .iter()
            .filter(|outcome| !outcome.passed())
            .map(|outcome| outcome.name.as_str())
    }
}

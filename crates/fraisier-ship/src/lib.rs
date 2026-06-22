//! Version management and the release workflow for the `fraisier` CLI.
//!
//! This crate backs `fraisier version show` / `version bump` and (the
//! [`ship`](crate::ship) workflow) `fraisier ship`. The version logic understands
//! both Rust (`Cargo.toml`) and Python (`pyproject.toml`) projects, editing the
//! file in place with `toml_edit` so formatting and comments survive a bump.

use std::path::{Path, PathBuf};

mod ship;
mod version;

pub use ship::{ship, ShipOptions, ShipReport};
pub use version::{
    bump, locate, next_version, show, version_in_toml, Bump, ProjectKind, VersionInfo,
};

/// An error from reading or bumping a project's version.
#[derive(Debug, thiserror::Error)]
pub enum ShipError {
    /// Neither `Cargo.toml` nor `pyproject.toml` was found in the directory.
    #[error("no version file in {0} (looked for Cargo.toml, pyproject.toml)")]
    NoVersionFile(PathBuf),
    /// The version file was found but carries no version field.
    #[error("{path}: no version field found ({detail})")]
    NoVersionField {
        /// The file inspected.
        path: PathBuf,
        /// Which tables were checked.
        detail: String,
    },
    /// The version file could not be parsed as TOML.
    #[error("{path}: invalid TOML: {source}")]
    Toml {
        /// The file that failed to parse.
        path: PathBuf,
        /// The underlying parse error.
        source: toml_edit::TomlError,
    },
    /// The current version string is not valid semver.
    #[error("{version:?} is not a valid semantic version: {source}")]
    Semver {
        /// The offending version string.
        version: String,
        /// The underlying semver parse error.
        source: semver::Error,
    },
    /// Reading the version file failed.
    #[error("reading {path}: {source}")]
    Read {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Writing the version file failed.
    #[error("writing {path}: {source}")]
    Write {
        /// The file that could not be written.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// `ship` refuses to run with uncommitted changes in the working tree.
    #[error("the working tree at {dir} has uncommitted changes; commit or stash them before ship")]
    DirtyWorkingTree {
        /// The repository directory.
        dir: PathBuf,
    },
    /// A `git` invocation failed (non-zero exit or could not be spawned).
    #[error("git {op} failed: {detail}")]
    Git {
        /// The git operation that failed (e.g. `commit`).
        op: String,
        /// The captured stderr or spawn error.
        detail: String,
    },
    /// Another ship advanced `origin/<branch>` past the version observed at the
    /// start of this run, so committing the locally-computed bump would push a
    /// duplicate-version commit that cannot fast-forward. The on-disk bump has been
    /// rolled back; the message carries a copy-pasteable rebase recipe.
    #[error(
        "version race on {branch}: this run started from {observed} but origin/{branch} is now \
         at {on_origin}. The version bump was rolled back. Reconcile and retry:\n  \
         git fetch origin {branch}\n  git rebase origin/{branch}\n  fraisier ship <patch|minor|major>"
    )]
    VersionRace {
        /// The version observed locally at the start of the run.
        observed: String,
        /// The version now present at `origin/<branch>`.
        on_origin: String,
        /// The branch the race was detected on.
        branch: String,
    },
}

/// Read `path` to a string, mapping I/O failure to [`ShipError::Read`].
pub(crate) fn read(path: &Path) -> Result<String, ShipError> {
    std::fs::read_to_string(path).map_err(|source| ShipError::Read {
        path: path.to_path_buf(),
        source,
    })
}

/// Write `contents` to `path`, mapping I/O failure to [`ShipError::Write`].
pub(crate) fn write(path: &Path, contents: &str) -> Result<(), ShipError> {
    std::fs::write(path, contents).map_err(|source| ShipError::Write {
        path: path.to_path_buf(),
        source,
    })
}

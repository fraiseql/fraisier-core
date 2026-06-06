//! Version management and the release workflow for the `fraisier` CLI.
//!
//! This crate backs `fraisier version show` / `version bump` and (the
//! [`ship`](crate::ship) workflow) `fraisier ship`. The version logic understands
//! both Rust (`Cargo.toml`) and Python (`pyproject.toml`) projects, editing the
//! file in place with `toml_edit` so formatting and comments survive a bump.

use std::path::{Path, PathBuf};

mod version;

pub use version::{bump, locate, next_version, show, Bump, ProjectKind, VersionInfo};

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

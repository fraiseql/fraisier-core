//! Reading and bumping a project's version in `Cargo.toml` / `pyproject.toml`.

use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

use crate::ShipError;

/// Which kind of project a version file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    /// A Rust project (`Cargo.toml`).
    Cargo,
    /// A Python project (`pyproject.toml`).
    PyProject,
}

impl ProjectKind {
    /// The file name a project of this kind keeps its version in.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Cargo => "Cargo.toml",
            Self::PyProject => "pyproject.toml",
        }
    }
}

/// Which component of a semantic version to increment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bump {
    /// `x.y.z` → `x+1.0.0`.
    Major,
    /// `x.y.z` → `x.y+1.0`.
    Minor,
    /// `x.y.z` → `x.y.z+1`.
    Patch,
}

/// A located project version: which file, what kind, and the current value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    /// Whether this is a Cargo or pyproject version.
    pub kind: ProjectKind,
    /// The file the version was read from.
    pub path: PathBuf,
    /// The current version string.
    pub version: String,
}

/// Compute the next version string after applying `level` to `current`.
///
/// A bump increments the requested component, zeroes the lower ones, and clears
/// any pre-release / build metadata (`1.2.3-rc.1` + patch → `1.2.4`).
///
/// # Errors
/// [`ShipError::Semver`] if `current` is not a valid semantic version.
pub fn next_version(current: &str, level: Bump) -> Result<String, ShipError> {
    let v = semver::Version::parse(current).map_err(|source| ShipError::Semver {
        version: current.to_owned(),
        source,
    })?;
    let next = match level {
        Bump::Major => semver::Version::new(v.major + 1, 0, 0),
        Bump::Minor => semver::Version::new(v.major, v.minor + 1, 0),
        Bump::Patch => semver::Version::new(v.major, v.minor, v.patch + 1),
    };
    Ok(next.to_string())
}

/// The table paths a version may live under, in precedence order, per kind.
const fn candidate_paths(kind: ProjectKind) -> &'static [&'static [&'static str]] {
    match kind {
        ProjectKind::Cargo => &[
            &["package", "version"],
            &["workspace", "package", "version"],
        ],
        ProjectKind::PyProject => &[&["project", "version"], &["tool", "poetry", "version"]],
    }
}

/// A human-readable list of the tables checked, for the not-found error.
fn describe_paths(kind: ProjectKind) -> String {
    candidate_paths(kind)
        .iter()
        .map(|path| format!("[{}]", path.join(".")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Navigate to the item at `path` (recursion mirrors [`navigate_mut`]).
fn navigate<'a>(item: &'a toml_edit::Item, path: &[&str]) -> Option<&'a toml_edit::Item> {
    match path.split_first() {
        None => Some(item),
        Some((head, rest)) => navigate(item.get(head)?, rest),
    }
}

/// Read the version string out of `doc` for the given project kind.
fn read_version(doc: &DocumentMut, kind: ProjectKind) -> Option<String> {
    for path in candidate_paths(kind) {
        if let Some(value) = navigate(doc.as_item(), path).and_then(toml_edit::Item::as_str) {
            return Some(value.to_owned());
        }
    }
    None
}

/// Navigate to the item at `path` mutably (recursion sidesteps the loop-reborrow
/// borrow-check limitation that mutable step-by-step navigation hits).
fn navigate_mut<'a>(
    item: &'a mut toml_edit::Item,
    path: &[&str],
) -> Option<&'a mut toml_edit::Item> {
    match path.split_first() {
        None => Some(item),
        Some((head, rest)) => navigate_mut(item.get_mut(head)?, rest),
    }
}

/// Set the version string at the first existing candidate path, preserving the
/// value's surrounding formatting and trailing comment. Returns whether one was
/// found and updated.
fn set_version(doc: &mut DocumentMut, kind: ProjectKind, new: &str) -> bool {
    for path in candidate_paths(kind) {
        let Some(item) = navigate_mut(doc.as_item_mut(), path) else {
            continue;
        };
        if let Some(value) = item.as_value_mut() {
            let decor = value.decor().clone();
            let mut formatted = toml_edit::Value::from(new);
            *formatted.decor_mut() = decor;
            *value = formatted;
            return true;
        }
    }
    false
}

/// Locate the project version: `Cargo.toml` if present, else `pyproject.toml`.
///
/// # Errors
/// [`ShipError::NoVersionFile`] if neither file exists, or a read/parse/field
/// error for the file that is present.
pub fn locate(dir: &Path) -> Result<VersionInfo, ShipError> {
    for kind in [ProjectKind::Cargo, ProjectKind::PyProject] {
        let path = dir.join(kind.file_name());
        if !path.exists() {
            continue;
        }
        let doc = parse(&path)?;
        let version = read_version(&doc, kind).ok_or_else(|| ShipError::NoVersionField {
            path: path.clone(),
            detail: describe_paths(kind),
        })?;
        return Ok(VersionInfo {
            kind,
            path,
            version,
        });
    }
    Err(ShipError::NoVersionFile(dir.to_path_buf()))
}

/// Read and parse a TOML file into an editable document.
fn parse(path: &Path) -> Result<DocumentMut, ShipError> {
    crate::read(path)?
        .parse::<DocumentMut>()
        .map_err(|source| ShipError::Toml {
            path: path.to_path_buf(),
            source,
        })
}

/// Read the current version without modifying anything (`version show`).
///
/// # Errors
/// As [`locate`].
pub fn show(dir: &Path) -> Result<VersionInfo, ShipError> {
    locate(dir)
}

/// Bump the project version in place, returning `(old, new)`. Preserves the
/// file's formatting and comments.
///
/// # Errors
/// As [`locate`], plus [`ShipError::Write`] / [`ShipError::Semver`].
pub fn bump(dir: &Path, level: Bump) -> Result<(String, String), ShipError> {
    let info = locate(dir)?;
    let new = next_version(&info.version, level)?;
    let mut doc = parse(&info.path)?;
    if !set_version(&mut doc, info.kind, &new) {
        return Err(ShipError::NoVersionField {
            path: info.path,
            detail: describe_paths(info.kind),
        });
    }
    crate::write(&info.path, &doc.to_string())?;
    Ok((info.version, new))
}

#[cfg(test)]
mod tests {
    use super::{bump, locate, next_version, Bump, ProjectKind};

    #[test]
    fn next_version_bumps_each_component() {
        assert_eq!(next_version("0.1.5", Bump::Patch).unwrap(), "0.1.6");
        assert_eq!(next_version("1.2.3", Bump::Minor).unwrap(), "1.3.0");
        assert_eq!(next_version("1.2.3", Bump::Major).unwrap(), "2.0.0");
    }

    #[test]
    fn next_version_clears_prerelease_and_build() {
        assert_eq!(next_version("1.0.0-alpha.1", Bump::Patch).unwrap(), "1.0.1");
        assert_eq!(next_version("1.0.0-beta.2", Bump::Minor).unwrap(), "1.1.0");
        assert_eq!(next_version("2.3.4+build.7", Bump::Major).unwrap(), "3.0.0");
    }

    #[test]
    fn next_version_rejects_non_semver() {
        assert!(next_version("not-a-version", Bump::Patch).is_err());
    }

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("write");
    }

    #[test]
    fn locate_reads_a_plain_cargo_package() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.4.2\"\n",
        );
        let info = locate(dir.path()).unwrap();
        assert_eq!(info.kind, ProjectKind::Cargo);
        assert_eq!(info.version, "0.4.2");
    }

    #[test]
    fn locate_reads_a_workspace_package_version() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"1.0.0-alpha.1\"\n",
        );
        let info = locate(dir.path()).unwrap();
        assert_eq!(info.version, "1.0.0-alpha.1");
    }

    #[test]
    fn locate_falls_back_to_pyproject() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            "[project]\nname = \"app\"\nversion = \"2.1.0\"\n",
        );
        let info = locate(dir.path()).unwrap();
        assert_eq!(info.kind, ProjectKind::PyProject);
        assert_eq!(info.version, "2.1.0");
    }

    #[test]
    fn locate_prefers_cargo_over_pyproject() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.4.2\"\n",
        );
        write(
            dir.path(),
            "pyproject.toml",
            "[project]\nversion = \"9.9.9\"\n",
        );
        let info = locate(dir.path()).unwrap();
        assert_eq!(info.kind, ProjectKind::Cargo);
        assert_eq!(info.version, "0.4.2");
    }

    #[test]
    fn locate_errors_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(locate(dir.path()).is_err());
    }

    #[test]
    fn bump_rewrites_in_place_preserving_comments() {
        let dir = tempfile::tempdir().unwrap();
        let body = "[package]\nname = \"app\"  # the app\nversion = \"0.1.5\" # current\n";
        write(dir.path(), "Cargo.toml", body);

        let (old, new) = bump(dir.path(), Bump::Patch).unwrap();
        assert_eq!(old, "0.1.5");
        assert_eq!(new, "0.1.6");

        let after = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert!(after.contains("version = \"0.1.6\""), "after: {after}");
        // Comments and the surrounding layout survive the edit.
        assert!(after.contains("# the app"), "comment preserved: {after}");
        assert!(after.contains("# current"), "comment preserved: {after}");
    }

    #[test]
    fn bump_rewrites_a_workspace_version() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace.package]\nversion = \"1.2.3\"\n",
        );
        let (_, new) = bump(dir.path(), Bump::Minor).unwrap();
        assert_eq!(new, "1.3.0");
        let after = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert!(after.contains("version = \"1.3.0\""), "after: {after}");
    }
}

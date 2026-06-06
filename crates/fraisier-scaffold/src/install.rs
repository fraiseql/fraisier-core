//! Writing generated files to disk and pruning stale fraisier-generated ones.
//!
//! `scaffold` writes the [`rel_path`](GeneratedFile::rel_path) tree into a local
//! output directory for review; `scaffold-install` writes the
//! [`install_dest`](GeneratedFile::install_dest) files to their system locations
//! (under an overridable `root` prefix, so tests and chroots stay sandboxed) and
//! optionally prunes stale files. Pruning only ever removes files that carry the
//! [`MARKER`] and are not part of the current generated set — it never touches a
//! file fraisier did not write.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::{GeneratedFile, ScaffoldError, MARKER};

/// Resolve an absolute install destination under `root` (so `/` is the real
/// system and a tempdir sandboxes the writes).
fn under_root(root: &Path, dest: &Path) -> PathBuf {
    root.join(dest.strip_prefix("/").unwrap_or(dest))
}

/// The rooted destinations [`install`] would write, without writing them — the
/// install half of the `scaffold-install --dry-run` plan.
#[must_use]
pub fn install_targets(files: &[GeneratedFile], root: &Path) -> Vec<PathBuf> {
    files
        .iter()
        .filter_map(|file| file.install_dest.as_deref())
        .map(|dest| under_root(root, dest))
        .collect()
}

/// Write each file's [`rel_path`](GeneratedFile::rel_path) under `out_dir`
/// (the `scaffold` command), creating parent directories. Returns the paths.
///
/// # Errors
/// [`ScaffoldError::Write`] if a file or its parent directory cannot be written.
pub fn write_tree(files: &[GeneratedFile], out_dir: &Path) -> Result<Vec<PathBuf>, ScaffoldError> {
    let mut written = Vec::with_capacity(files.len());
    for file in files {
        let path = out_dir.join(&file.rel_path);
        write_file(&path, &file.contents)?;
        written.push(path);
    }
    Ok(written)
}

/// Install the system files (those with an `install_dest`) under `root`,
/// creating parent directories. Returns the paths written.
///
/// # Errors
/// [`ScaffoldError::Write`] if a file or its parent directory cannot be written.
pub fn install(files: &[GeneratedFile], root: &Path) -> Result<Vec<PathBuf>, ScaffoldError> {
    let mut installed = Vec::new();
    for file in files {
        if let Some(dest) = &file.install_dest {
            let path = under_root(root, dest);
            write_file(&path, &file.contents)?;
            installed.push(path);
        }
    }
    Ok(installed)
}

/// Compute the stale fraisier-generated files: marker-bearing files in the
/// install directories that are not part of the current generated set.
///
/// # Errors
/// [`ScaffoldError::Read`] if an install directory cannot be scanned.
pub fn prune_plan(files: &[GeneratedFile], root: &Path) -> Result<Vec<PathBuf>, ScaffoldError> {
    // The files this generation owns (rooted), and the directories they live in.
    let current: BTreeSet<PathBuf> = files
        .iter()
        .filter_map(|file| file.install_dest.as_deref())
        .map(|dest| under_root(root, dest))
        .collect();
    let dirs: BTreeSet<PathBuf> = current
        .iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect();

    let mut stale = Vec::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(ScaffoldError::Read { path: dir, source }),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Never prune a file we are about to (re)write, and only ever prune
            // files carrying the marker — hand-written files are left untouched.
            if !current.contains(&path) && is_fraisier_generated(&path) {
                stale.push(path);
            }
        }
    }
    stale.sort();
    Ok(stale)
}

/// Whether `path` is a regular file that carries the fraisier marker.
fn is_fraisier_generated(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|contents| contents.contains(MARKER))
}

/// The systemd unit directory the installer targets (under the `root` prefix).
const SYSTEMD_DIR: &str = "/etc/systemd/system";

/// List the fraisier-installed (marker-bearing) unit files in the systemd dir.
///
/// Scans `/etc/systemd/system` under `root` — what `scheduled list` enumerates.
/// Returns sorted paths; an absent directory yields an empty list.
///
/// # Errors
/// [`ScaffoldError::Read`] if the systemd directory exists but cannot be scanned.
pub fn list_installed(root: &Path) -> Result<Vec<PathBuf>, ScaffoldError> {
    let dir = under_root(root, Path::new(SYSTEMD_DIR));
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(ScaffoldError::Read { path: dir, source }),
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_fraisier_generated(path))
        .collect();
    found.sort();
    Ok(found)
}

/// Remove exactly the files in `files` (their `install_dest` under `root`).
///
/// The inverse of [`install`]. A file is removed **only** if it carries the
/// [`MARKER`]; a hand-written file at the same path is left untouched. Returns
/// the removed paths.
///
/// # Errors
/// [`ScaffoldError::Write`] if a marker-bearing file cannot be removed.
pub fn uninstall(files: &[GeneratedFile], root: &Path) -> Result<Vec<PathBuf>, ScaffoldError> {
    let mut removed = Vec::new();
    for file in files {
        let Some(dest) = &file.install_dest else {
            continue;
        };
        let path = under_root(root, dest);
        if path.exists() && is_fraisier_generated(&path) {
            std::fs::remove_file(&path).map_err(|source| ScaffoldError::Write {
                path: path.clone(),
                source,
            })?;
            removed.push(path);
        }
    }
    Ok(removed)
}

/// Remove the files in `stale` (the output of [`prune_plan`]).
///
/// # Errors
/// [`ScaffoldError::Write`] if a file cannot be removed.
pub fn prune(stale: &[PathBuf]) -> Result<(), ScaffoldError> {
    for path in stale {
        std::fs::remove_file(path).map_err(|source| ScaffoldError::Write {
            path: path.clone(),
            source,
        })?;
    }
    Ok(())
}

/// Write `contents` to `path`, creating parent directories first.
fn write_file(path: &Path, contents: &str) -> Result<(), ScaffoldError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ScaffoldError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, contents).map_err(|source| ScaffoldError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::{install, prune, prune_plan, write_tree};
    use crate::{generate, MARKER};
    use fraisier_config::DeployConfig;

    const CFG: &str = r#"
[deploy]
name = "checkout"
environment = "production"

[artifact]
source = "release"
release_url = "https://x/checkout-{version}.tar.gz"
checksum_url = "https://x/checkout-{version}.tar.gz.sha256"
active_path = "/srv/checkout/current"

[migration]
adapter = "confiture"
database_url_env = "CHECKOUT_DATABASE_URL"

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"

[lb]
adapter = "nginx"
config_path = "/etc/nginx/sites-available/checkout"
upstream = "checkout_upstream"
"#;

    fn files() -> Vec<crate::GeneratedFile> {
        generate(&DeployConfig::from_toml_str(CFG).expect("parse")).expect("generate")
    }

    #[test]
    fn write_tree_writes_the_rel_path_tree() {
        let out = tempfile::tempdir().unwrap();
        let written = write_tree(&files(), out.path()).expect("write_tree");
        assert_eq!(written.len(), 5);
        assert!(out.path().join("systemd/checkout.service").exists());
        assert!(out.path().join("nginx/checkout.conf").exists());
        assert!(out.path().join(".github/workflows/deploy.yml").exists());
    }

    #[test]
    fn install_writes_system_files_under_root() {
        let root = tempfile::tempdir().unwrap();
        let written = install(&files(), root.path()).expect("install");
        // Three system files (service, socket, nginx); repo files are skipped.
        assert_eq!(written.len(), 3);
        let service = root.path().join("etc/systemd/system/checkout.service");
        assert!(service.exists(), "service installed under root");
        assert!(std::fs::read_to_string(&service).unwrap().contains(MARKER));
        assert!(root
            .path()
            .join("etc/nginx/sites-available/checkout")
            .exists());
    }

    #[test]
    fn prune_plan_finds_only_stale_marker_files() {
        let root = tempfile::tempdir().unwrap();
        install(&files(), root.path()).expect("install");
        let systemd = root.path().join("etc/systemd/system");

        // A stale fraisier file (carries the marker, not in the current set).
        std::fs::write(
            systemd.join("old-thing.service"),
            format!("# {MARKER}\n[Unit]\n"),
        )
        .unwrap();
        // A hand-written file (no marker) must be left alone.
        std::fs::write(systemd.join("keep.service"), "[Unit]\nDescription=mine\n").unwrap();

        let stale = prune_plan(&files(), root.path()).expect("prune_plan");
        assert_eq!(stale.len(), 1, "only the stale marker file: {stale:?}");
        assert!(stale[0].ends_with("old-thing.service"));
    }

    #[test]
    fn prune_removes_only_the_listed_files() {
        let root = tempfile::tempdir().unwrap();
        install(&files(), root.path()).expect("install");
        let systemd = root.path().join("etc/systemd/system");
        std::fs::write(systemd.join("old.service"), format!("# {MARKER}\n")).unwrap();
        std::fs::write(systemd.join("keep.service"), "mine\n").unwrap();

        let stale = prune_plan(&files(), root.path()).expect("plan");
        prune(&stale).expect("prune");

        assert!(!systemd.join("old.service").exists(), "stale removed");
        assert!(systemd.join("keep.service").exists(), "non-marker kept");
        assert!(
            systemd.join("checkout.service").exists(),
            "current file kept"
        );
    }

    /// A scheduled `backup` config (the safe value; needs no unattended opt-in).
    fn scheduled_files() -> Vec<crate::GeneratedFile> {
        let toml = format!("{CFG}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"backup\"\n");
        crate::generate_scheduled(
            &fraisier_config::DeployConfig::from_toml_str(&toml).expect("parse"),
        )
        .expect("generate scheduled")
    }

    #[test]
    fn install_then_list_then_uninstall_round_trips_clean() {
        use super::{list_installed, uninstall};
        let root = tempfile::tempdir().unwrap();
        let files = scheduled_files();

        // Nothing installed yet.
        assert!(list_installed(root.path()).expect("list").is_empty());

        install(&files, root.path()).expect("install");
        let listed = list_installed(root.path()).expect("list");
        assert_eq!(listed.len(), 2, "timer + service listed: {listed:?}");
        assert!(listed
            .iter()
            .any(|p| p.to_string_lossy().ends_with("scheduled.timer")));

        let removed = uninstall(&files, root.path()).expect("uninstall");
        assert_eq!(removed.len(), 2, "both removed: {removed:?}");
        assert!(
            list_installed(root.path()).expect("list").is_empty(),
            "systemd dir clean after uninstall"
        );
    }

    #[test]
    fn uninstall_never_removes_a_hand_written_file() {
        use super::uninstall;
        let root = tempfile::tempdir().unwrap();
        let files = scheduled_files();
        install(&files, root.path()).expect("install");

        // A hand-written file sitting where a generated one would: same dir, no
        // marker. Overwrite the installed timer with a non-marker body.
        let timer = root
            .path()
            .join("etc/systemd/system/fraisier-checkout-production-scheduled.timer");
        std::fs::write(&timer, "[Timer]\nOnCalendar=hourly\n").unwrap();

        let removed = uninstall(&files, root.path()).expect("uninstall");
        // Only the (still-marker-bearing) service is removed; the de-marked timer stays.
        assert_eq!(
            removed.len(),
            1,
            "only the marker file removed: {removed:?}"
        );
        assert!(timer.exists(), "the de-marked file is left untouched");
    }
}

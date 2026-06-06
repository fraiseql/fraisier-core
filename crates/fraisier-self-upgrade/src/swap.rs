//! The on-disk binary layout `apply` manages, and its atomic swap mechanics.
//!
//! The supervised unit's `ExecStart` points at a stable `current` symlink inside
//! a `bin/` directory of staged binaries. A swap stages the new binary beside the
//! others and **atomically repoints** the symlink (write a temp link, then
//! `rename` over it — the same temp-then-rename the artifact axis uses), so the
//! unit never observes a half-updated `current`. Revert is the same operation
//! pointed back at the kept-old target, and `prune` reaps stale binaries (never
//! the active one) only after a healthy commit.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use fraisier_adapter_support::staging;

use crate::Error;

/// The adapter tag used when delegating to the shared staging helpers.
const TAG: &str = "self-upgrade";

/// The `bin/` directory plus the `current` symlink within it.
#[derive(Debug, Clone)]
pub struct Layout {
    bin_dir: PathBuf,
    current: PathBuf,
}

impl Layout {
    /// A layout rooted at `bin_dir`, with `current` the swap symlink inside it.
    #[must_use]
    pub fn new(bin_dir: impl Into<PathBuf>) -> Self {
        let bin_dir = bin_dir.into();
        let current = bin_dir.join("current");
        Self { bin_dir, current }
    }

    /// The `current` symlink path (what a unit's `ExecStart` points at).
    #[must_use]
    pub fn current(&self) -> &Path {
        &self.current
    }

    /// The basename the `current` symlink resolves to, or `None` if nothing is
    /// active yet. This is the **keep-old** capture used before a swap.
    ///
    /// # Errors
    /// [`Error::Io`] if the link exists but cannot be read.
    pub fn active(&self) -> Result<Option<String>, Error> {
        staging::read_active_link(&self.current, TAG, "active")
            .map(|opt| opt.map(|artifact| artifact.id))
            .map_err(|e| Error::Io(e.message))
    }

    /// The full path a staged binary named `id` lives at.
    #[must_use]
    pub fn staged_path(&self, id: &str) -> PathBuf {
        self.bin_dir.join(id)
    }

    /// Write `bytes` as an executable staged binary named `id`; returns its path.
    ///
    /// # Errors
    /// [`Error::Io`] if the directory cannot be created or the binary written.
    pub fn stage(&self, id: &str, bytes: &[u8]) -> Result<PathBuf, Error> {
        std::fs::create_dir_all(&self.bin_dir)
            .map_err(|e| Error::Io(format!("creating {}: {e}", self.bin_dir.display())))?;
        let path = self.staged_path(id);
        std::fs::write(&path, bytes)
            .map_err(|e| Error::Io(format!("writing {}: {e}", path.display())))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| Error::Io(format!("chmod +x {}: {e}", path.display())))?;
        Ok(path)
    }

    /// Atomically repoint `current` at the staged binary named `id`.
    ///
    /// # Errors
    /// [`Error::Io`] if the symlink swap fails (the prior link is left intact).
    pub fn activate(&self, id: &str) -> Result<(), Error> {
        let staged = self.staged_path(id);
        staging::activate_symlink(&self.current, &staged, TAG, "activate")
            .map_err(|e| Error::Io(e.message))
    }

    /// Reap staged binaries beyond the `keep` most-recently-modified, **never**
    /// removing the currently-active target (the `current` symlink and any
    /// in-progress temp links are also left alone). Returns the removed paths.
    ///
    /// # Errors
    /// [`Error::Io`] if the directory cannot be read.
    pub fn prune(&self, keep: usize) -> Result<Vec<PathBuf>, Error> {
        let active = self.active()?;
        let mut staged: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
        let entries = std::fs::read_dir(&self.bin_dir)
            .map_err(|e| Error::Io(format!("reading {}: {e}", self.bin_dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::Io(format!("reading dir entry: {e}")))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Skip the symlink itself and our atomic-swap temp links.
            if name == "current" || name.starts_with("current.fraisier-tmp-") {
                continue;
            }
            // Skip anything that is not a regular file (e.g. a stray symlink).
            let meta = entry
                .metadata()
                .map_err(|e| Error::Io(format!("stat {}: {e}", entry.path().display())))?;
            if !meta.is_file() {
                continue;
            }
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            staged.push((entry.path(), mtime));
        }
        // Newest first; keep the first `keep`, plus the active one regardless.
        staged.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
        let mut removed = Vec::new();
        for (index, (path, _)) in staged.iter().enumerate() {
            let is_active = active
                .as_deref()
                .is_some_and(|id| path.file_name().is_some_and(|name| name == id));
            if index < keep || is_active {
                continue;
            }
            std::fs::remove_file(path)
                .map_err(|e| Error::Io(format!("removing {}: {e}", path.display())))?;
            removed.push(path.clone());
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::Layout;
    use std::os::unix::fs::PermissionsExt as _;

    fn set_mtime(path: &std::path::Path, secs: u64) {
        let when = filetime::FileTime::from_unix_time(i64::try_from(secs).unwrap(), 0);
        filetime::set_file_mtime(path, when).expect("set mtime");
    }

    #[test]
    fn stage_writes_an_executable_then_activate_swaps_the_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(dir.path().join("bin"));

        assert_eq!(layout.active().expect("active"), None, "nothing active yet");

        layout.stage("1.0.0", b"#!/bin/sh\ntrue\n").expect("stage");
        let mode = std::fs::metadata(layout.staged_path("1.0.0"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "owner/group/other exec bits set");

        layout.activate("1.0.0").expect("activate");
        assert_eq!(layout.active().expect("active").as_deref(), Some("1.0.0"));

        // The keep-old capture: a second stage+activate, with the prior visible.
        layout.stage("1.1.0", b"new\n").expect("stage 2");
        let prior = layout.active().expect("active").expect("some");
        assert_eq!(prior, "1.0.0", "prior captured before the swap");
        layout.activate("1.1.0").expect("activate 2");
        assert_eq!(layout.active().expect("active").as_deref(), Some("1.1.0"));
    }

    #[test]
    fn prune_keeps_the_newest_n_and_never_the_active() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(dir.path().join("bin"));
        for (id, mtime) in [("a", 100), ("b", 200), ("c", 300), ("d", 400)] {
            layout.stage(id, b"x").expect("stage");
            set_mtime(&layout.staged_path(id), mtime);
        }
        // Make the *oldest* binary the active one, so prune must spare it even
        // though it falls outside the newest-2 window.
        layout.activate("a").expect("activate oldest");

        let removed = layout.prune(2).expect("prune");
        let names: Vec<String> = removed
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // Newest-2 = d, c (kept); active = a (kept); only b is reaped.
        assert_eq!(names, vec!["b".to_owned()], "removed: {names:?}");
        assert!(
            layout.staged_path("a").exists(),
            "active must survive prune"
        );
        assert!(layout.staged_path("c").exists());
        assert!(layout.staged_path("d").exists());
        assert!(!layout.staged_path("b").exists());
    }
}

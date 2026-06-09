//! Shared artifact staging mechanics.
//!
//! Every on-disk artifact adapter (`release`, `local`, `git`) stages a versioned
//! copy and then points an `active_path` symlink at it. Only *how the bytes
//! arrive* differs; the activation swap and the "what is active now" read are
//! identical, so they live here once.

use std::path::Path;

use fraisier_core::adapter_axes::{AdapterError, AdapterErrorKind, ArtifactRef};

use crate::error;

/// Atomically point `active_path` at `staged_path`: create a temp symlink beside
/// the target, then `rename` it over the target, so `active_path` is never
/// observed half-updated.
///
/// # Errors
/// [`AdapterError`] of kind [`AdapterErrorKind::Execution`] if the symlink or the
/// rename fails.
pub fn activate_symlink(
    active_path: &Path,
    staged_path: &Path,
    adapter: &str,
    operation: &str,
) -> Result<(), AdapterError> {
    let fail = |message: String| {
        error(
            AdapterErrorKind::Execution,
            adapter,
            operation,
            message,
            None,
        )
    };
    let tmp = active_path.with_file_name(format!(
        "{}.fraisier-tmp-{}",
        active_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("current"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(staged_path, &tmp)
        .map_err(|cause| fail(format!("failed to create symlink: {cause}")))?;
    std::fs::rename(&tmp, active_path).map_err(|cause| {
        let _ = std::fs::remove_file(&tmp);
        fail(format!("failed to swap active symlink: {cause}"))
    })
}

/// Read the artifact currently active at `active_path` — the symlink target's
/// basename — or `None` if nothing has been activated yet.
///
/// # Errors
/// [`AdapterError`] of kind [`AdapterErrorKind::Execution`] if the link cannot be
/// read for a reason other than absence.
pub fn read_active_link(
    active_path: &Path,
    adapter: &str,
    operation: &str,
) -> Result<Option<ArtifactRef>, AdapterError> {
    match std::fs::read_link(active_path) {
        Ok(target) => Ok(target
            .file_name()
            .and_then(|name| name.to_str())
            .map(|id| ArtifactRef {
                id: id.to_owned(),
                checksum: None,
            })),
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(cause) => Err(error(
            AdapterErrorKind::Execution,
            adapter,
            operation,
            format!("failed to read active link: {cause}"),
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{activate_symlink, read_active_link};

    #[test]
    fn activate_then_read_round_trips_the_basename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged = dir.path().join("releases/1.2.3");
        std::fs::create_dir_all(&staged).expect("staged");
        let active = dir.path().join("current");

        // Nothing active yet.
        assert_eq!(
            read_active_link(&active, "test", "current").expect("read"),
            None
        );

        activate_symlink(&active, &staged, "test", "activate").expect("activate");
        let now = read_active_link(&active, "test", "current")
            .expect("read")
            .expect("some");
        assert_eq!(now.id, "1.2.3");

        // Re-activating over an existing link swaps cleanly.
        let staged2 = dir.path().join("releases/1.2.4");
        std::fs::create_dir_all(&staged2).expect("staged2");
        activate_symlink(&active, &staged2, "test", "activate").expect("reactivate");
        assert_eq!(
            read_active_link(&active, "test", "current")
                .expect("read")
                .expect("some")
                .id,
            "1.2.4"
        );
    }
}

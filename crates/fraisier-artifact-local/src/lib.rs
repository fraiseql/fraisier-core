//! # fraisier-artifact-local
//!
//! The [`LocalArtifact`] adapter: an [`ArtifactAdapter`] that deploys an
//! already-built artifact from a local path. It stages a versioned *copy* of the
//! path under the staging directory (so prior versions remain for rollback) and
//! activates it via the shared atomic symlink swap — the `release` adapter minus
//! the download and checksum (PRD §6.3, the `local` artifact source).
//!
//! ## Configuration
//!
//! Read per call from [`AdapterCtx::settings`] (the `[artifact]` table):
//!
//! ```toml
//! [artifact]
//! source = "local"
//! path = "/builds/app"              # the already-built file or directory
//! version = "1.2.3"                 # staged subdir name; defaults to path basename
//! staging_dir = "/var/lib/app/staging"   # default <workdir>/.fraisier-staging
//! active_path = "/var/lib/app/current"   # the symlink swapped on activate
//! ```

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use fraisier_adapter_support::{error, staging};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterError, AdapterErrorKind, ArtifactAdapter, ArtifactRef, HostId,
    StagedArtifact,
};
use serde_json::Value;

/// The adapter's identity name.
const ADAPTER_NAME: &str = "local";

/// Resolved local-artifact configuration.
#[derive(Debug)]
struct Config {
    path: PathBuf,
    version: String,
    staging_dir: PathBuf,
    active_path: Option<PathBuf>,
}

impl Config {
    fn from_ctx(ctx: &AdapterCtx, operation: &str) -> Result<Self, AdapterError> {
        let path = string_setting(ctx, "path")
            .map(PathBuf::from)
            .ok_or_else(|| invalid(operation, "no 'path' configured in [artifact] settings"))?;
        // Default the staged version name to the source's basename.
        let version = string_setting(ctx, "version").unwrap_or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("local")
                .to_owned()
        });
        let staging_dir = string_setting(ctx, "staging_dir")
            .map_or_else(|| ctx.workdir.join(".fraisier-staging"), PathBuf::from);
        Ok(Self {
            path,
            version,
            staging_dir,
            active_path: string_setting(ctx, "active_path").map(PathBuf::from),
        })
    }

    fn staged_path(&self) -> PathBuf {
        self.staging_dir.join(&self.version)
    }

    fn require_active_path(&self, operation: &str) -> Result<&Path, AdapterError> {
        self.active_path.as_deref().ok_or_else(|| {
            invalid(
                operation,
                "no 'active_path' configured in [artifact] settings",
            )
        })
    }
}

fn string_setting(ctx: &AdapterCtx, key: &str) -> Option<String> {
    ctx.settings
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn invalid(operation: &str, message: &str) -> AdapterError {
    error(
        AdapterErrorKind::InvalidConfig,
        ADAPTER_NAME,
        operation,
        message.to_owned(),
        None,
    )
}

fn execution(operation: &str, message: String) -> AdapterError {
    error(
        AdapterErrorKind::Execution,
        ADAPTER_NAME,
        operation,
        message,
        None,
    )
}

/// Recursively copy `src` (a file or directory) to `dst`.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// The local-artifact adapter.
///
/// # Example
/// ```
/// use fraisier_artifact_local::LocalArtifact;
///
/// let adapter = LocalArtifact::new();
/// let _ = adapter;
/// ```
#[derive(Default)]
pub struct LocalArtifact {
    _private: (),
}

impl LocalArtifact {
    /// Create a local-artifact adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

#[async_trait]
impl ArtifactAdapter for LocalArtifact {
    async fn stage(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
    ) -> Result<StagedArtifact, AdapterError> {
        let cfg = Config::from_ctx(ctx, "stage")?;
        if !cfg.path.exists() {
            return Err(execution(
                "stage",
                format!("source path does not exist: {}", cfg.path.display()),
            ));
        }
        let staged_path = cfg.staged_path();
        // Replace any prior staged copy of this version, then copy fresh.
        let _ = std::fs::remove_dir_all(&staged_path);
        let _ = std::fs::remove_file(&staged_path);
        copy_tree(&cfg.path, &staged_path)
            .map_err(|e| execution("stage", format!("failed to stage local artifact: {e}")))?;

        Ok(StagedArtifact {
            artifact: ArtifactRef {
                id: cfg.version,
                checksum: None,
            },
            path: staged_path,
        })
    }

    async fn activate(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
        staged: &StagedArtifact,
    ) -> Result<(), AdapterError> {
        let cfg = Config::from_ctx(ctx, "activate")?;
        let active = cfg.require_active_path("activate")?;
        staging::activate_symlink(active, &staged.path, ADAPTER_NAME, "activate")
    }

    async fn current(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
    ) -> Result<Option<ArtifactRef>, AdapterError> {
        let cfg = Config::from_ctx(ctx, "current")?;
        let active = cfg.require_active_path("current")?;
        staging::read_active_link(active, ADAPTER_NAME, "current")
    }
}

#[cfg(test)]
mod tests {
    use super::LocalArtifact;
    use fraisier_core::adapter_axes::{AdapterCtx, ArtifactAdapter as _, HostId};
    use serde_json::json;

    fn ctx_with(source: &std::path::Path, staging: &std::path::Path, version: &str) -> AdapterCtx {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.settings
            .insert("path".to_owned(), json!(source.display().to_string()));
        ctx.settings.insert(
            "staging_dir".to_owned(),
            json!(staging.display().to_string()),
        );
        ctx.settings.insert("version".to_owned(), json!(version));
        ctx
    }

    #[tokio::test]
    async fn stage_copies_a_directory_into_the_versioned_staging_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("build");
        std::fs::create_dir_all(src.join("bin")).expect("src");
        std::fs::write(src.join("bin/app"), "binary").expect("bin");
        let staging = dir.path().join("staging");

        let ctx = ctx_with(&src, &staging, "1.2.3");
        let staged = LocalArtifact::new()
            .stage(&ctx, &HostId::new("localhost"))
            .await
            .expect("stage");

        assert_eq!(staged.artifact.id, "1.2.3");
        assert_eq!(staged.path, staging.join("1.2.3"));
        // The directory was copied verbatim.
        assert_eq!(
            std::fs::read_to_string(staging.join("1.2.3/bin/app")).unwrap(),
            "binary"
        );
    }

    #[tokio::test]
    async fn stage_then_activate_points_current_at_the_staged_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("build");
        std::fs::create_dir_all(&src).expect("src");
        std::fs::write(src.join("file"), "x").expect("file");
        let staging = dir.path().join("staging");
        let active = dir.path().join("current");

        let mut ctx = ctx_with(&src, &staging, "9.9.9");
        ctx.settings.insert(
            "active_path".to_owned(),
            json!(active.display().to_string()),
        );
        let adapter = LocalArtifact::new();
        let host = HostId::new("localhost");

        let staged = adapter.stage(&ctx, &host).await.expect("stage");
        adapter
            .activate(&ctx, &host, &staged)
            .await
            .expect("activate");
        let now = adapter.current(&ctx, &host).await.expect("current");
        assert_eq!(now.expect("some").id, "9.9.9");
    }

    #[tokio::test]
    async fn stage_defaults_version_to_the_source_basename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("my-build");
        std::fs::create_dir_all(&src).expect("src");
        let staging = dir.path().join("staging");
        let mut ctx = AdapterCtx::new("app", "prod");
        ctx.settings
            .insert("path".to_owned(), json!(src.display().to_string()));
        ctx.settings.insert(
            "staging_dir".to_owned(),
            json!(staging.display().to_string()),
        );

        let staged = LocalArtifact::new()
            .stage(&ctx, &HostId::new("localhost"))
            .await
            .expect("stage");
        assert_eq!(staged.artifact.id, "my-build");
    }

    #[tokio::test]
    async fn stage_errors_when_the_source_path_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx_with(
            &dir.path().join("absent"),
            &dir.path().join("staging"),
            "1.0.0",
        );
        assert!(LocalArtifact::new()
            .stage(&ctx, &HostId::new("localhost"))
            .await
            .is_err());
    }
}

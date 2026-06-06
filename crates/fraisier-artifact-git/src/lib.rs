//! # fraisier-artifact-git
//!
//! The [`GitArtifact`] adapter: an [`ArtifactAdapter`] that stages a git checkout
//! and activates it via the shared atomic symlink swap (PRD §6.3, the `git`
//! artifact source).
//!
//! Staging clones the repo into a temp directory, checks out the configured ref,
//! resolves the commit sha, then moves it to a versioned staged path (named by
//! the configured `version`, or the sha). Cloning shells out to `git`, so it uses
//! the host's credentials/SSH config for private repositories.
//!
//! ## Configuration
//!
//! Read per call from [`AdapterCtx::settings`] (the `[artifact]` table):
//!
//! ```toml
//! [artifact]
//! source = "git"
//! repo = "https://github.com/org/app.git"   # required
//! ref = "v1.2.3"                             # branch / tag / sha; default branch if unset
//! version = "1.2.3"                          # staged subdir name; defaults to the sha
//! staging_dir = "/var/lib/app/staging"      # default <workdir>/.fraisier-staging
//! active_path = "/var/lib/app/current"      # the symlink swapped on activate
//! ```

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use fraisier_adapter_support::{error, run_command, staging};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterError, AdapterErrorKind, ArtifactAdapter, ArtifactRef, HostId,
    StagedArtifact,
};
use serde_json::Value;

/// The adapter's identity name.
const ADAPTER_NAME: &str = "git";

/// Resolved git-artifact configuration.
#[derive(Debug)]
struct Config {
    repo: String,
    reference: Option<String>,
    version: Option<String>,
    staging_dir: PathBuf,
    active_path: Option<PathBuf>,
}

impl Config {
    fn from_ctx(ctx: &AdapterCtx, operation: &str) -> Result<Self, AdapterError> {
        let repo = string_setting(ctx, "repo")
            .ok_or_else(|| invalid(operation, "no 'repo' configured in [artifact] settings"))?;
        let staging_dir = string_setting(ctx, "staging_dir")
            .map_or_else(|| ctx.workdir.join(".fraisier-staging"), PathBuf::from);
        Ok(Self {
            repo,
            reference: string_setting(ctx, "ref"),
            version: string_setting(ctx, "version"),
            staging_dir,
            active_path: string_setting(ctx, "active_path").map(PathBuf::from),
        })
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

/// Run `git args` (in `cwd` if given), returning stdout. A non-zero exit is an
/// execution error carrying git's stderr.
async fn git(
    args: &[OsString],
    cwd: Option<&Path>,
    operation: &str,
) -> Result<String, AdapterError> {
    let captured = run_command(OsStr::new("git"), args, &[], cwd, ADAPTER_NAME, operation).await?;
    if captured.succeeded() {
        Ok(captured.stdout)
    } else {
        Err(error(
            AdapterErrorKind::Execution,
            ADAPTER_NAME,
            operation,
            format!("git failed: {}", captured.stderr.trim()),
            captured.stderr_opt(),
        ))
    }
}

/// `OsString` from a string slice (terse argv construction).
fn os(value: &str) -> OsString {
    OsString::from(value)
}

/// The git-artifact adapter.
///
/// # Example
/// ```
/// use fraisier_artifact_git::GitArtifact;
///
/// let adapter = GitArtifact::new();
/// let _ = adapter;
/// ```
#[derive(Default)]
pub struct GitArtifact {
    _private: (),
}

impl GitArtifact {
    /// Create a git-artifact adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

#[async_trait]
impl ArtifactAdapter for GitArtifact {
    async fn stage(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
    ) -> Result<StagedArtifact, AdapterError> {
        let cfg = Config::from_ctx(ctx, "stage")?;
        std::fs::create_dir_all(&cfg.staging_dir)
            .map_err(|e| execution("stage", format!("failed to create staging dir: {e}")))?;

        // Clone into a temp dir, check out the ref, then resolve the sha — the
        // final staged dir is named by `version` or the sha, so we only know it
        // after the checkout.
        let tmp = cfg.staging_dir.join(".fraisier-git-tmp");
        let _ = std::fs::remove_dir_all(&tmp);
        git(
            &[os("clone"), os(&cfg.repo), tmp.clone().into_os_string()],
            None,
            "stage",
        )
        .await?;
        if let Some(reference) = &cfg.reference {
            git(&[os("checkout"), os(reference)], Some(&tmp), "stage").await?;
        }
        let sha = git(&[os("rev-parse"), os("HEAD")], Some(&tmp), "stage")
            .await?
            .trim()
            .to_owned();

        let id = cfg.version.clone().unwrap_or_else(|| sha.clone());
        let target = cfg.staging_dir.join(&id);
        let _ = std::fs::remove_dir_all(&target);
        std::fs::rename(&tmp, &target)
            .map_err(|e| execution("stage", format!("failed to move staged checkout: {e}")))?;

        Ok(StagedArtifact {
            artifact: ArtifactRef {
                id,
                checksum: Some(sha),
            },
            path: target,
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
    use super::GitArtifact;
    use fraisier_core::adapter_axes::{AdapterCtx, ArtifactAdapter as _, HostId};
    use serde_json::json;
    use std::path::Path;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A source repo with one commit on `main` and content "v1".
    fn source_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        run(dir.path(), &["init", "-b", "main"]);
        run(dir.path(), &["config", "user.email", "t@example.com"]);
        run(dir.path(), &["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("app.txt"), "v1").expect("write");
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-m", "one"]);
        dir
    }

    fn head_sha(dir: &Path) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn ctx(repo: &Path, staging: &Path) -> AdapterCtx {
        let mut ctx = AdapterCtx::new("app", "production");
        ctx.settings
            .insert("repo".to_owned(), json!(repo.display().to_string()));
        ctx.settings.insert(
            "staging_dir".to_owned(),
            json!(staging.display().to_string()),
        );
        ctx
    }

    #[tokio::test]
    async fn stage_clones_and_records_the_sha() {
        let src = source_repo();
        let work = tempfile::tempdir().expect("work");
        let staging = work.path().join("staging");
        let mut c = ctx(src.path(), &staging);
        c.settings.insert("version".to_owned(), json!("1.0.0"));

        let staged = GitArtifact::new()
            .stage(&c, &HostId::new("localhost"))
            .await
            .expect("stage");
        assert_eq!(staged.artifact.id, "1.0.0");
        assert_eq!(
            staged.artifact.checksum.as_deref(),
            Some(head_sha(src.path()).as_str())
        );
        assert_eq!(
            std::fs::read_to_string(staged.path.join("app.txt")).unwrap(),
            "v1"
        );
    }

    #[tokio::test]
    async fn stage_checks_out_the_requested_ref() {
        let src = source_repo();
        // A second commit on a branch with different content.
        run(src.path(), &["checkout", "-b", "feature"]);
        std::fs::write(src.path().join("app.txt"), "v2").expect("write");
        run(src.path(), &["commit", "-am", "two"]);
        run(src.path(), &["checkout", "main"]);

        let work = tempfile::tempdir().expect("work");
        let mut c = ctx(src.path(), &work.path().join("staging"));
        c.settings.insert("ref".to_owned(), json!("feature"));

        let staged = GitArtifact::new()
            .stage(&c, &HostId::new("localhost"))
            .await
            .expect("stage");
        assert_eq!(
            std::fs::read_to_string(staged.path.join("app.txt")).unwrap(),
            "v2",
            "the feature branch was checked out"
        );
    }

    #[tokio::test]
    async fn stage_defaults_the_id_to_the_sha() {
        let src = source_repo();
        let work = tempfile::tempdir().expect("work");
        let staged = GitArtifact::new()
            .stage(
                &ctx(src.path(), &work.path().join("staging")),
                &HostId::new("localhost"),
            )
            .await
            .expect("stage");
        assert_eq!(staged.artifact.id, head_sha(src.path()));
    }

    #[tokio::test]
    async fn stage_then_activate_round_trips_current() {
        let src = source_repo();
        let work = tempfile::tempdir().expect("work");
        let active = work.path().join("current");
        let mut c = ctx(src.path(), &work.path().join("staging"));
        c.settings.insert("version".to_owned(), json!("7.7.7"));
        c.settings.insert(
            "active_path".to_owned(),
            json!(active.display().to_string()),
        );

        let adapter = GitArtifact::new();
        let host = HostId::new("localhost");
        let staged = adapter.stage(&c, &host).await.expect("stage");
        adapter
            .activate(&c, &host, &staged)
            .await
            .expect("activate");
        assert_eq!(
            adapter
                .current(&c, &host)
                .await
                .expect("current")
                .expect("some")
                .id,
            "7.7.7"
        );
    }

    #[tokio::test]
    async fn stage_errors_on_a_bad_repo() {
        let work = tempfile::tempdir().expect("work");
        let c = ctx(Path::new("/no/such/repo"), &work.path().join("staging"));
        assert!(GitArtifact::new()
            .stage(&c, &HostId::new("localhost"))
            .await
            .is_err());
    }
}

//! The `ship` release workflow: bump the version, commit it, push, and report
//! whether a deploy should follow.
//!
//! The deploy step itself is left to the caller (the CLI), so this crate stays
//! free of the deploy machinery — `ship` reports `deploy_requested` and the CLI
//! runs `deploy` when it is set. All git work shells out via [`Command`] with
//! separate arguments — never a shell string.

use std::path::Path;
use std::process::Command;

use crate::{version, Bump, ShipError};

/// How a `ship` run should behave.
#[derive(Debug, Clone)]
pub struct ShipOptions {
    /// Compute and report the plan without writing, committing, or pushing.
    pub dry_run: bool,
    /// Skip the follow-on deploy (`deploy_requested` is reported `false`).
    pub no_deploy: bool,
    /// Push the release commit to `remote` after committing.
    pub push: bool,
    /// The git remote to push to.
    pub remote: String,
    /// The commit-message template; `{version}` is replaced with the new version.
    pub message_template: Option<String>,
}

impl Default for ShipOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            no_deploy: false,
            push: true,
            remote: "origin".to_owned(),
            message_template: None,
        }
    }
}

/// The outcome of a `ship` run (or, with `dry_run`, the plan it would execute).
// Reason: each bool reports an independent step result (committed / pushed /
// deploy-requested / dry-run); folding them into enums would be more ceremony
// than signal for a flat status report.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShipReport {
    /// The version before the bump.
    pub old_version: String,
    /// The version after the bump.
    pub new_version: String,
    /// The commit message used (or that would be used).
    pub message: String,
    /// Whether a release commit was made.
    pub committed: bool,
    /// Whether the commit was pushed.
    pub pushed: bool,
    /// Whether the caller should run a deploy next.
    pub deploy_requested: bool,
    /// Whether this was a dry run (nothing was written).
    pub dry_run: bool,
}

/// The default commit-message template when none is configured.
const DEFAULT_MESSAGE: &str = "Release v{version}";

/// Render the release commit message for `new_version`.
fn render_message(opts: &ShipOptions, new_version: &str) -> String {
    opts.message_template
        .as_deref()
        .unwrap_or(DEFAULT_MESSAGE)
        .replace("{version}", new_version)
}

/// Run the `ship` workflow against the project in `dir`.
///
/// On a real run: requires a clean working tree, bumps the version file, commits
/// just that file with the rendered message, and (unless `push` is off) pushes
/// to the configured remote. A `dry_run` computes the plan without side effects.
///
/// # Errors
/// [`ShipError::DirtyWorkingTree`] if the tree is dirty, [`ShipError::Git`] if a
/// git step fails, or any error from locating/bumping the version.
pub fn ship(dir: &Path, level: Bump, opts: &ShipOptions) -> Result<ShipReport, ShipError> {
    let info = version::locate(dir)?;
    let new_version = version::next_version(&info.version, level)?;
    let message = render_message(opts, &new_version);
    let deploy_requested = !opts.no_deploy;

    if opts.dry_run {
        return Ok(ShipReport {
            old_version: info.version,
            new_version,
            message,
            committed: false,
            pushed: false,
            deploy_requested,
            dry_run: true,
        });
    }

    // Preflight: a dirty tree would sweep unrelated changes into the release
    // commit, so refuse before touching anything.
    if !git_clean(dir)? {
        return Err(ShipError::DirtyWorkingTree {
            dir: dir.to_path_buf(),
        });
    }

    let (old_version, new_version) = version::bump(dir, level)?;
    // Stage only the version file so the release commit is exactly the bump.
    git(dir, &["add", info.kind.file_name()], "add")?;
    git(dir, &["commit", "-m", &message], "commit")?;
    let pushed = if opts.push {
        git(dir, &["push", &opts.remote, "HEAD"], "push")?;
        true
    } else {
        false
    };

    Ok(ShipReport {
        old_version,
        new_version,
        message,
        committed: true,
        pushed,
        deploy_requested,
        dry_run: false,
    })
}

/// Run `git args` in `dir`, returning stdout or a tagged [`ShipError::Git`].
fn git(dir: &Path, args: &[&str], op: &str) -> Result<String, ShipError> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|error| ShipError::Git {
            op: op.to_owned(),
            detail: format!("spawning git: {error}"),
        })?;
    if !output.status.success() {
        return Err(ShipError::Git {
            op: op.to_owned(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether the working tree has no uncommitted changes.
fn git_clean(dir: &Path) -> Result<bool, ShipError> {
    Ok(git(dir, &["status", "--porcelain"], "status")?
        .trim()
        .is_empty())
}

#[cfg(test)]
mod tests {
    use super::{ship, ShipOptions};
    use crate::Bump;
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

    /// A temp dir initialised as a git repo with one committed Cargo.toml.
    fn repo(version: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        run(dir.path(), &["init", "-b", "main"]);
        run(dir.path(), &["config", "user.email", "t@example.com"]);
        run(dir.path(), &["config", "user.name", "Test"]);
        std::fs::write(
            dir.path().join("Cargo.toml"),
            format!("[package]\nname = \"app\"\nversion = \"{version}\"\n"),
        )
        .expect("write");
        run(dir.path(), &["add", "Cargo.toml"]);
        run(dir.path(), &["commit", "-m", "init"]);
        dir
    }

    fn last_subject(dir: &Path, refname: &str) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["log", "-1", "--format=%s", refname])
            .output()
            .expect("git log");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[test]
    fn dry_run_computes_the_plan_without_writing() {
        let dir = repo("0.1.5");
        let opts = ShipOptions {
            dry_run: true,
            push: false,
            ..ShipOptions::default()
        };
        let report = ship(dir.path(), Bump::Patch, &opts).expect("dry run");
        assert_eq!(report.new_version, "0.1.6");
        assert_eq!(report.message, "Release v0.1.6");
        assert!(!report.committed && !report.pushed);
        assert!(report.dry_run);
        // The version file is untouched.
        let body = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert!(body.contains("0.1.5"), "unchanged: {body}");
    }

    #[test]
    fn ship_bumps_commits_and_pushes() {
        let dir = repo("0.1.5");
        // A bare repo to push into.
        let remote = tempfile::tempdir().expect("remote");
        run(remote.path(), &["init", "--bare", "-b", "main"]);
        run(
            dir.path(),
            &["remote", "add", "origin", &remote.path().to_string_lossy()],
        );

        let opts = ShipOptions {
            no_deploy: true,
            ..ShipOptions::default()
        };
        let report = ship(dir.path(), Bump::Patch, &opts).expect("ship");
        assert_eq!(report.old_version, "0.1.5");
        assert_eq!(report.new_version, "0.1.6");
        assert!(report.committed && report.pushed);
        assert!(!report.deploy_requested, "no_deploy was set");

        // The release commit landed locally and on the remote.
        assert_eq!(last_subject(dir.path(), "HEAD"), "Release v0.1.6");
        assert_eq!(last_subject(remote.path(), "main"), "Release v0.1.6");
        // The file is bumped and committed (clean tree afterwards).
        let body = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert!(body.contains("0.1.6"), "bumped: {body}");
    }

    #[test]
    fn ship_refuses_a_dirty_working_tree() {
        let dir = repo("0.1.5");
        std::fs::write(dir.path().join("dirty.txt"), "uncommitted").unwrap();
        let opts = ShipOptions {
            push: false,
            ..ShipOptions::default()
        };
        let err = ship(dir.path(), Bump::Patch, &opts).expect_err("dirty");
        assert!(
            matches!(err, crate::ShipError::DirtyWorkingTree { .. }),
            "got {err:?}"
        );
        // The version was not bumped.
        let body = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert!(body.contains("0.1.5"), "unchanged: {body}");
    }
}

//! # fraisier-sync (experimental)
//!
//! Share the deploy **ledger** across operators over git refs, with no bespoke
//! server: each fraise/env's state lives as a commit chain under
//! `refs/fraisier/sync/<key>` on a git remote the team already has.
//!
//! ## Why git refs
//!
//! Git gives optimistic concurrency for free: a `push` that is not a
//! fast-forward is **rejected**, which is exactly the conflict check a shared
//! mutable ledger needs. fraisier never force-pushes — a rejected push surfaces
//! as [`PushOutcome::Conflict`], and the operator reconciles with [`pull_state`]
//! (accept remote) before re-pushing. The commit chain is the history.
//!
//! A **persistent local bare repo** (`sync_dir`) holds each ref at the
//! last-synced commit. That local ref is the sync base: a new push parents on it,
//! so if the remote has moved since, git rejects the push rather than silently
//! overwriting another operator's state.
//!
//! ## Experimental
//!
//! The on-ref format (a `state.json` blob per commit) is **not** a stability
//! commitment in v1.0 — it may change before GA. The CLI warns on use.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// The ref namespace shared state lives under.
pub const REF_NAMESPACE: &str = "refs/fraisier/sync";

/// The single file each sync commit stores the serialized state in.
const STATE_FILE: &str = "state.json";

/// An error from a sync operation.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// `git` could not be launched.
    #[error("could not run git: {0}")]
    Spawn(#[source] std::io::Error),
    /// A `git` invocation failed.
    #[error("git {op} failed: {detail}")]
    Git {
        /// The operation that failed.
        op: String,
        /// Captured stderr.
        detail: String,
    },
    /// The sync key is not a valid ref component.
    #[error("invalid sync key '{0}' (must be non-empty and free of whitespace)")]
    InvalidKey(String),
}

/// The result of [`push_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// A new (or updated) ref was pushed to the remote.
    Pushed,
    /// The remote already held this exact state — nothing to do.
    UpToDate,
    /// The push was rejected as a non-fast-forward: the remote diverged. The
    /// state was **not** forced. `local_head` is the commit we tried to push and
    /// `remote_head` is the remote's current tip — the divergence, for the caller
    /// to surface. Reconcile with [`pull_state`], then push again.
    Conflict {
        /// The local commit that was rejected (what we tried to push).
        local_head: String,
        /// The remote ref's current commit (what the remote already has).
        remote_head: String,
    },
}

/// Push the `state_json` for `key` to the remote, appending a commit to
/// `refs/fraisier/sync/<key>` (parent = the local sync base).
///
/// # Errors
/// [`SyncError`] if the key is invalid or a git operation other than a
/// non-fast-forward rejection fails. A rejection is returned as
/// [`PushOutcome::Conflict`], not an error.
pub fn push_state(
    sync_dir: &Path,
    remote: &str,
    key: &str,
    state_json: &str,
) -> Result<PushOutcome, SyncError> {
    validate_key(key)?;
    ensure_repo(sync_dir)?;
    let refname = refname(key);

    // Reuse the current tip when the state is unchanged (no empty commits); else
    // build a new commit parented on the local sync base.
    let parent = rev_parse(sync_dir, &refname)?;
    let unchanged =
        parent.is_some() && read_state(sync_dir, &refname)?.as_deref() == Some(state_json);
    let commit = match parent.as_deref() {
        Some(tip) if unchanged => tip.to_owned(),
        _ => {
            let blob = hash_object(sync_dir, state_json)?;
            let tree = mktree(sync_dir, &blob)?;
            commit_tree(
                sync_dir,
                &tree,
                parent.as_deref(),
                &format!("fraisier sync {key}"),
            )?
        }
    };
    git(
        sync_dir,
        &["update-ref", &refname, &commit],
        None,
        "update-ref",
    )?;

    push(sync_dir, remote, &refname, &commit)
}

/// Fetch the remote state for `key` into the local sync base (accepting the
/// remote — this is the conflict-resolution side), returning the state JSON, or
/// `None` if the remote has no such ref.
///
/// # Errors
/// [`SyncError`] if the key is invalid or a git operation fails.
pub fn pull_state(sync_dir: &Path, remote: &str, key: &str) -> Result<Option<String>, SyncError> {
    validate_key(key)?;
    ensure_repo(sync_dir)?;
    let refname = refname(key);
    if !remote_has(remote, &refname)? {
        return Ok(None);
    }
    // Force the local base to the remote: pulling is an explicit "accept remote".
    git(
        sync_dir,
        &["fetch", "-q", remote, &format!("+{refname}:{refname}")],
        None,
        "fetch",
    )?;
    read_state(sync_dir, &refname)
}

/// The keys (`<fraise>/<env>`) present under the sync namespace on `remote`.
///
/// # Errors
/// [`SyncError`] if `git ls-remote` fails.
pub fn remote_keys(remote: &str) -> Result<Vec<String>, SyncError> {
    let out = git_capture(None, &["ls-remote", remote], None, "ls-remote")?;
    let prefix = format!("{REF_NAMESPACE}/");
    let mut keys = Vec::new();
    for line in String::from_utf8_lossy(&out).lines() {
        if let Some((_, refname)) = line.split_once('\t') {
            if let Some(key) = refname.strip_prefix(&prefix) {
                keys.push(key.to_owned());
            }
        }
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

/// Delete the sync ref for `key` on `remote` (orphan reclaim). The commits are
/// left for the remote's `gc` to reap.
///
/// # Errors
/// [`SyncError`] if the key is invalid or the delete push fails.
pub fn delete_remote(remote: &str, key: &str) -> Result<(), SyncError> {
    validate_key(key)?;
    git_capture(
        None,
        &["push", remote, "--delete", &refname(key)],
        None,
        "push --delete",
    )?;
    Ok(())
}

// --------------------------------------------------------------------------
// git plumbing
// --------------------------------------------------------------------------

fn refname(key: &str) -> String {
    format!("{REF_NAMESPACE}/{key}")
}

fn validate_key(key: &str) -> Result<(), SyncError> {
    if key.is_empty() || key.chars().any(char::is_whitespace) {
        return Err(SyncError::InvalidKey(key.to_owned()));
    }
    Ok(())
}

/// `git init --bare` the sync repo (idempotent).
fn ensure_repo(sync_dir: &Path) -> Result<(), SyncError> {
    let dir = sync_dir.to_string_lossy().into_owned();
    git_capture(None, &["init", "--bare", "-q", &dir], None, "init")?;
    Ok(())
}

/// The commit a ref points at, or `None` if the ref is absent.
fn rev_parse(sync_dir: &Path, refname: &str) -> Result<Option<String>, SyncError> {
    let mut command = base_git(Some(sync_dir));
    command.args(["rev-parse", "-q", "--verify", refname]);
    let output = command.output().map_err(SyncError::Spawn)?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ))
    } else {
        Ok(None)
    }
}

/// Read the `state.json` blob a ref points at, or `None` if absent.
fn read_state(sync_dir: &Path, refname: &str) -> Result<Option<String>, SyncError> {
    let mut command = base_git(Some(sync_dir));
    command.args(["cat-file", "blob", &format!("{refname}:{STATE_FILE}")]);
    let output = command.output().map_err(SyncError::Spawn)?;
    if output.status.success() {
        Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
    } else {
        Ok(None)
    }
}

fn hash_object(sync_dir: &Path, content: &str) -> Result<String, SyncError> {
    let out = git_capture(
        Some(sync_dir),
        &["hash-object", "-w", "--stdin"],
        Some(content.as_bytes()),
        "hash-object",
    )?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

fn mktree(sync_dir: &Path, blob: &str) -> Result<String, SyncError> {
    let entry = format!("100644 blob {blob}\t{STATE_FILE}\n");
    let out = git_capture(
        Some(sync_dir),
        &["mktree"],
        Some(entry.as_bytes()),
        "mktree",
    )?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

fn commit_tree(
    sync_dir: &Path,
    tree: &str,
    parent: Option<&str>,
    message: &str,
) -> Result<String, SyncError> {
    let mut args = vec!["commit-tree", tree, "-m", message];
    if let Some(parent) = parent {
        args.push("-p");
        args.push(parent);
    }
    let out = git_capture(Some(sync_dir), &args, None, "commit-tree")?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

/// The argv for a plain, **non-forcing** push. Factored out and locked by a test
/// so a forcing flag or `+refspec` can never slip in: a divergent push must
/// always surface as a [`PushOutcome::Conflict`], never clobber the remote.
const fn push_args<'a>(remote: &'a str, refname: &'a str) -> [&'a str; 3] {
    ["push", remote, refname]
}

/// Push `refname` to `remote`, mapping a non-fast-forward rejection onto
/// [`PushOutcome::Conflict`] (never forcing). `local_head` is the commit being
/// pushed, surfaced in the conflict so the caller can show the divergence.
fn push(
    sync_dir: &Path,
    remote: &str,
    refname: &str,
    local_head: &str,
) -> Result<PushOutcome, SyncError> {
    let mut command = base_git(Some(sync_dir));
    command.args(push_args(remote, refname));
    let output = command.output().map_err(SyncError::Spawn)?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        if combined.contains("up-to-date") {
            return Ok(PushOutcome::UpToDate);
        }
        return Ok(PushOutcome::Pushed);
    }
    if combined.contains("rejected")
        && (combined.contains("non-fast-forward") || combined.contains("fetch first"))
    {
        let remote_head = ls_remote_head(remote, refname)?.unwrap_or_default();
        return Ok(PushOutcome::Conflict {
            local_head: local_head.to_owned(),
            remote_head,
        });
    }
    Err(SyncError::Git {
        op: "push".to_owned(),
        detail: combined.trim().to_owned(),
    })
}

/// Whether `remote` has `refname`.
fn remote_has(remote: &str, refname: &str) -> Result<bool, SyncError> {
    Ok(ls_remote_head(remote, refname)?.is_some())
}

/// The commit `remote` has for `refname`, or `None`.
fn ls_remote_head(remote: &str, refname: &str) -> Result<Option<String>, SyncError> {
    let out = git_capture(None, &["ls-remote", remote, refname], None, "ls-remote")?;
    let text = String::from_utf8_lossy(&out);
    Ok(text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .map(ToOwned::to_owned))
}

/// A `git` command with a deterministic commit identity and optional `--git-dir`.
fn base_git(git_dir: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(dir) = git_dir {
        command.arg("--git-dir");
        command.arg(dir.as_os_str());
    }
    command
        .env("GIT_AUTHOR_NAME", "fraisier")
        .env("GIT_AUTHOR_EMAIL", "fraisier@localhost")
        .env("GIT_COMMITTER_NAME", "fraisier")
        .env("GIT_COMMITTER_EMAIL", "fraisier@localhost");
    command
}

/// Run a `git` command (no captured success output expected); error on failure.
fn git(git_dir: &Path, args: &[&str], stdin: Option<&[u8]>, op: &str) -> Result<(), SyncError> {
    git_capture(Some(git_dir), args, stdin, op).map(|_| ())
}

/// Run a `git` command, returning its stdout; error (with stderr) on failure.
fn git_capture(
    git_dir: Option<&Path>,
    args: &[&str],
    stdin: Option<&[u8]>,
    op: &str,
) -> Result<Vec<u8>, SyncError> {
    let mut command = base_git(git_dir);
    command.args(args.iter().map(OsStr::new));
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().map_err(SyncError::Spawn)?;
    if let Some(data) = stdin {
        child
            .stdin
            .take()
            .expect("stdin piped")
            .write_all(data)
            .map_err(SyncError::Spawn)?;
    }
    let output = child.wait_with_output().map_err(SyncError::Spawn)?;
    if !output.status.success() {
        return Err(SyncError::Git {
            op: op.to_owned(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::{pull_state, push_state, remote_keys, PushOutcome};
    use std::path::Path;
    use std::process::Command;

    /// A bare repo standing in for the shared remote.
    fn bare_remote(at: &Path) -> String {
        let status = Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(at)
            .status()
            .expect("git init");
        assert!(status.success());
        at.to_string_lossy().into_owned()
    }

    #[test]
    fn push_then_pull_round_trips_state_via_the_remote() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = bare_remote(&tmp.path().join("remote.git"));
        let operator_a = tmp.path().join("a.git");
        let operator_b = tmp.path().join("b.git");
        let key = "checkout/staging";

        // Operator A pushes state.
        let pushed = push_state(&operator_a, &remote, key, r#"{"rev":"1"}"#).expect("push");
        assert_eq!(pushed, PushOutcome::Pushed);

        // Pushing the same state again is a no-op at the remote.
        let again = push_state(&operator_a, &remote, key, r#"{"rev":"1"}"#).expect("push");
        assert_eq!(again, PushOutcome::UpToDate);

        // Operator B (a fresh sync base) pulls A's state.
        let pulled = pull_state(&operator_b, &remote, key).expect("pull");
        assert_eq!(pulled.as_deref(), Some(r#"{"rev":"1"}"#));

        // The remote lists the key.
        assert_eq!(remote_keys(&remote).expect("keys"), vec![key.to_owned()]);
    }

    #[test]
    fn a_divergent_push_is_a_conflict_not_a_clobber() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = bare_remote(&tmp.path().join("remote.git"));
        let operator_a = tmp.path().join("a.git");
        let operator_b = tmp.path().join("b.git");
        let key = "checkout/production";

        // A establishes the remote ref.
        assert_eq!(
            push_state(&operator_a, &remote, key, r#"{"rev":"a"}"#).expect("a push"),
            PushOutcome::Pushed
        );

        // B, with an independent sync base (root commit, no shared ancestor),
        // tries to push — git rejects the non-fast-forward; B does NOT clobber A.
        let outcome = push_state(&operator_b, &remote, key, r#"{"rev":"b"}"#).expect("b push");
        let PushOutcome::Conflict {
            local_head,
            remote_head,
        } = outcome
        else {
            panic!("expected a Conflict, got {outcome:?}");
        };
        // The divergence is surfaced: B's rejected commit vs the remote's tip,
        // and they genuinely differ (no shared ancestor).
        assert!(!local_head.is_empty(), "local head surfaced");
        assert!(!remote_head.is_empty(), "remote head surfaced");
        assert_ne!(local_head, remote_head, "the heads diverge");

        // A's state still stands on the remote.
        let still = pull_state(&tmp.path().join("c.git"), &remote, key).expect("pull");
        assert_eq!(still.as_deref(), Some(r#"{"rev":"a"}"#));
    }

    #[test]
    fn push_uses_a_plain_non_forcing_push() {
        // The argv is exactly `push <remote> <ref>` — no `--force`, no `+refspec`.
        assert_eq!(
            super::push_args("origin", "refs/fraisier/sync/x/y"),
            ["push", "origin", "refs/fraisier/sync/x/y"]
        );
    }

    #[test]
    fn no_force_push_path_exists_in_production_source() {
        // Locks the absence: a divergent push must always surface as a Conflict.
        // Scan only the production half (before the test module, which necessarily
        // names the flag) and build the needle at runtime so this test's own text
        // can never match. The sole force in the crate is the `+refspec` *fetch* in
        // pull_state (accept-remote into the LOCAL base), never a push to the remote.
        let src = include_str!("lib.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let force_flag = format!("--{}", "force");
        assert!(
            !production.contains(&force_flag),
            "no force-push flag may appear in production sync code"
        );
    }

    #[test]
    fn pull_is_none_when_the_remote_has_no_such_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = bare_remote(&tmp.path().join("remote.git"));
        let pulled = pull_state(&tmp.path().join("a.git"), &remote, "absent/key").expect("pull");
        assert!(pulled.is_none());
    }
}

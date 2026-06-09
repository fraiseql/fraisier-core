//! # fraisier-bootstrap
//!
//! SSH-based host bootstrap (PRD §3.4): prepare a host's filesystem so a deploy
//! can stage and activate artifacts on it.
//!
//! It reuses the [`Transport`] the deploy uses, so the *same* mechanism runs a
//! single-host config's preparation locally and a multi-host config's over `ssh`
//! per host. For beta the scope is deliberately narrow — create the directories
//! a deploy needs (`[artifact].staging_dir` and the directory that holds
//! `active_path`). Package/unit installation stays with `scaffold-install` and
//! the operator (the PRD allows a subprocess fallback for beta).

use std::ffi::{OsStr, OsString};

use fraisier_adapter_support::{error, Transport};
use fraisier_core::adapter_axes::{AdapterCtx, AdapterError, AdapterErrorKind};

/// The axis name carried on errors and OTel spans.
const ADAPTER_NAME: &str = "bootstrap";

/// The directories a deploy needs to exist on a host.
///
/// These are the artifact **staging** directory (where releases are placed
/// before activation) and the directory that holds the **active** symlink
/// (`active_path`'s parent, where the atomic swap happens). Returns an empty,
/// deduplicated, sorted list when neither path is configured — a single-host
/// config that stages under defaults has nothing host-specific to prepare.
#[must_use]
pub fn deploy_dirs(staging_dir: Option<&str>, active_path: Option<&str>) -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(staging) = staging_dir.filter(|s| !s.is_empty()) {
        dirs.push(staging.to_owned());
    }
    if let Some(active) = active_path.filter(|s| !s.is_empty()) {
        if let Some(parent) = std::path::Path::new(active).parent() {
            if !parent.as_os_str().is_empty() {
                dirs.push(parent.to_string_lossy().into_owned());
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Ensure `dirs` exist on the host reached via `transport` + `ctx`, with one
/// `mkdir -p`. A no-op when `dirs` is empty.
///
/// For [`Transport::Local`] this runs locally; for [`Transport::Ssh`] it runs on
/// the host the `ctx` names (`ctx.host` / `ctx.settings["address"]`). `mkdir -p`
/// is idempotent, so re-bootstrapping a host is safe.
///
/// # Errors
/// [`AdapterError`] if the `mkdir` cannot be spawned or exits non-zero.
pub async fn ensure_dirs(
    transport: &Transport,
    ctx: &AdapterCtx,
    dirs: &[String],
) -> Result<(), AdapterError> {
    if dirs.is_empty() {
        return Ok(());
    }
    let mut args: Vec<OsString> = vec![OsString::from("-p")];
    args.extend(dirs.iter().map(OsString::from));
    let captured = transport
        .run(
            ctx,
            OsStr::new("mkdir"),
            &args,
            &[],
            None,
            ADAPTER_NAME,
            "bootstrap",
        )
        .await?;
    if captured.succeeded() {
        return Ok(());
    }
    let code = captured
        .code
        .map_or_else(|| "signal".to_owned(), |code| code.to_string());
    Err(error(
        AdapterErrorKind::Execution,
        ADAPTER_NAME,
        "bootstrap",
        format!("`mkdir -p …` exited with {code}"),
        captured.stderr_opt(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{deploy_dirs, ensure_dirs};
    use fraisier_adapter_support::Transport;
    use fraisier_core::adapter_axes::AdapterCtx;

    #[test]
    fn deploy_dirs_derives_staging_and_the_active_parent() {
        let dirs = deploy_dirs(Some("/var/lib/app/releases"), Some("/srv/app/current"));
        assert!(
            dirs.contains(&"/var/lib/app/releases".to_owned()),
            "{dirs:?}"
        );
        assert!(
            dirs.contains(&"/srv/app".to_owned()),
            "active parent: {dirs:?}"
        );
        assert_eq!(dirs.len(), 2);
    }

    #[test]
    fn deploy_dirs_is_empty_when_nothing_is_configured() {
        assert!(deploy_dirs(None, None).is_empty());
        assert!(deploy_dirs(Some(""), Some("")).is_empty());
    }

    #[test]
    fn deploy_dirs_dedups_overlapping_paths() {
        // staging dir equals the active symlink's parent → a single entry.
        let dirs = deploy_dirs(Some("/srv/app"), Some("/srv/app/current"));
        assert_eq!(dirs, vec!["/srv/app".to_owned()]);
    }

    #[tokio::test]
    async fn ensure_dirs_creates_directories_via_the_local_transport() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let releases = tmp.path().join("releases");
        let current_parent = tmp.path().join("srv");
        let dirs = vec![
            releases.to_string_lossy().into_owned(),
            current_parent.to_string_lossy().into_owned(),
        ];

        ensure_dirs(&Transport::Local, &AdapterCtx::new("app", "test"), &dirs)
            .await
            .expect("mkdir -p");

        assert!(releases.is_dir(), "staging dir created");
        assert!(current_parent.is_dir(), "active parent created");
    }

    #[tokio::test]
    async fn ensure_dirs_is_a_noop_on_an_empty_list() {
        ensure_dirs(&Transport::Local, &AdapterCtx::new("app", "test"), &[])
            .await
            .expect("empty is ok");
    }
}

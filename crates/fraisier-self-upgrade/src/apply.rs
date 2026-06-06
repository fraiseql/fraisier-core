//! The `self-upgrade apply` controller: fetch → verify → swap → restart →
//! health-check → (on failure) auto-revert.
//!
//! The controller drives the supervised unit entirely **out of process** — a
//! [`Supervisor`] (`systemctl`) and a [`Health`] probe (HTTP `GET /healthz`). It
//! **never `exec`s the swapped binary**, so the revert is performed by the
//! supervisor, not by the possibly-broken new binary, and therefore survives a
//! binary that boots-then-dies (the headline failure path).

use std::time::Duration;

use async_trait::async_trait;

use crate::notify::{FailurePayload, Notifier};
use crate::{Layout, Source};

/// Drives the supervised unit. Out-of-process by construction (e.g. `systemctl`)
/// — see the load-bearing invariant in the crate docs.
#[async_trait]
pub trait Supervisor: Send + Sync {
    /// Restart the unit. Coordinated: the webhook drains its in-flight request on
    /// `SIGTERM` before exiting, so a restart never cuts off a deploy mid-flight.
    ///
    /// # Errors
    /// A human-readable message if the restart could not be issued.
    async fn restart(&self) -> Result<(), String>;

    /// Whether the supervisor reports the unit active (`systemctl is-active`).
    async fn is_active(&self) -> bool;
}

/// An out-of-process readiness probe — an HTTP `GET /healthz` against the
/// running server (never an in-process call into the swapped binary).
#[async_trait]
pub trait Health: Send + Sync {
    /// Whether the server answers `/healthz` with 200.
    async fn healthy(&self) -> bool;
}

/// What `apply` resolved to.
///
/// The CLI maps each to an exit code: `Committed` → 0, everything else →
/// non-zero. Every non-`Committed`, non-`AbortedBeforeSwap` outcome has already
/// fired a failure notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The new binary came up healthy; stale binaries were pruned to the keep-N.
    Committed {
        /// The id of the now-active binary.
        id: String,
        /// How many stale binaries were reaped.
        pruned: usize,
    },
    /// The new binary failed to come up healthy; the kept-old binary was
    /// reactivated and is healthy again. Non-zero exit; notification fired.
    Reverted {
        /// The id of the binary that failed.
        failed: String,
        /// The kept-old id service was restored to.
        restored: String,
        /// Why the new binary was rejected.
        reason: String,
    },
    /// The deepest failure path: the new binary failed **and** the revert target
    /// is also unhealthy (or there is none). Terminal — no further swap or restart
    /// is attempted. Non-zero exit; notification fired. The operator must step in.
    ManualIntervention {
        /// What went wrong, including that the revert could not restore service.
        reason: String,
    },
    /// Aborted **before** any swap (non-systemd target, fetch failure, checksum
    /// mismatch, or a staging IO error). The prior binary is untouched. Non-zero
    /// exit; no notification (nothing was disturbed — this is a clean refusal).
    AbortedBeforeSwap {
        /// Why the upgrade was refused before touching anything.
        reason: String,
    },
}

/// Everything `apply` needs that is *not* a live system handle.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Where the new binary's bytes come from (+ its checksum).
    pub source: Source,
    /// The on-disk binary layout (the `bin/` dir + `current` symlink).
    pub layout: Layout,
    /// An explicit version id for the staged binary; falls back to a digest tag.
    pub version: Option<String>,
    /// How many binaries to retain after a healthy commit (incl. the active one).
    pub keep: usize,
    /// Whether this is a systemd-managed deployment. `false` ⇒ refuse (no
    /// half-swap), because the auto-revert is systemd-managed.
    pub systemd_available: bool,
    /// Bound on the post-restart health probe before it is judged a failed start.
    pub health_timeout: Duration,
    /// How often to poll readiness within the timeout.
    pub poll_interval: Duration,
}

/// Run an upgrade end to end. Never returns `Err`: every expected failure is a
/// terminal [`ApplyOutcome`] with a defined exit-code mapping and (where service
/// was disturbed) a fired notification.
pub async fn apply(
    plan: &Plan,
    supervisor: &dyn Supervisor,
    health: &dyn Health,
    notifier: &dyn Notifier,
) -> ApplyOutcome {
    // Gate (b): the auto-revert is systemd-managed; refuse a non-systemd target
    // rather than perform an unprotectable half-swap.
    if !plan.systemd_available {
        return ApplyOutcome::AbortedBeforeSwap {
            reason: "not a systemd-managed deployment: `self-upgrade apply` needs \
                     systemd to perform the coordinated restart and auto-revert"
                .to_owned(),
        };
    }

    // Gate (a): fetch + verify-or-abort. A checksum mismatch (or any fetch/IO
    // failure) aborts here, before anything on disk is swapped.
    let verified = match plan.source.fetch().await {
        Ok(verified) => verified,
        Err(error) => {
            return ApplyOutcome::AbortedBeforeSwap {
                reason: error.to_string(),
            }
        }
    };
    if !verified.verified {
        tracing::warn!("self-upgrade: no checksum configured — staging an UNVERIFIED binary");
    }

    let id = resolve_id(plan.version.as_deref(), &verified.digest);

    // Capture the keep-old target *before* the swap, so revert has somewhere to go.
    let prior = match plan.layout.active() {
        Ok(prior) => prior,
        Err(error) => {
            return ApplyOutcome::AbortedBeforeSwap {
                reason: error.to_string(),
            }
        }
    };

    // Stage + atomically swap. Until the swap lands these are abort-before-swap
    // failures (the prior binary is still live and untouched).
    if let Err(error) = plan.layout.stage(&id, &verified.bytes) {
        return ApplyOutcome::AbortedBeforeSwap {
            reason: error.to_string(),
        };
    }
    if let Err(error) = plan.layout.activate(&id) {
        return ApplyOutcome::AbortedBeforeSwap {
            reason: error.to_string(),
        };
    }

    // The swap has landed. From here a failure triggers auto-revert, not abort.
    if bring_up(plan, supervisor, health).await {
        let pruned = plan.layout.prune(plan.keep).map_or_else(
            |error| {
                // A commit is healthy; a prune failure is not worth failing it.
                tracing::warn!("self-upgrade: prune after commit failed: {error}");
                0
            },
            |removed| removed.len(),
        );
        return ApplyOutcome::Committed { id, pruned };
    }

    revert(plan, supervisor, health, notifier, &id, prior.as_deref()).await
}

/// Repoint to the kept-old target and bring it back up. The new binary failed;
/// this is the recovery path. Exactly **one** revert restart is attempted — there
/// is no loop — and if the revert target is also unhealthy (or absent) the result
/// is the terminal manual-intervention state.
async fn revert(
    plan: &Plan,
    supervisor: &dyn Supervisor,
    health: &dyn Health,
    notifier: &dyn Notifier,
    failed: &str,
    prior: Option<&str>,
) -> ApplyOutcome {
    let Some(prior) = prior else {
        let reason = format!(
            "new binary '{failed}' failed health-check and there is no kept-old \
             binary to revert to"
        );
        notify(
            notifier,
            "self-upgrade-manual-intervention",
            Some(failed),
            None,
            &reason,
        )
        .await;
        return ApplyOutcome::ManualIntervention { reason };
    };

    if let Err(error) = plan.layout.activate(prior) {
        let reason = format!(
            "new binary '{failed}' failed health-check and the revert to '{prior}' \
             could not repoint the symlink: {error}"
        );
        notify(
            notifier,
            "self-upgrade-manual-intervention",
            Some(failed),
            None,
            &reason,
        )
        .await;
        return ApplyOutcome::ManualIntervention { reason };
    }

    if bring_up(plan, supervisor, health).await {
        let reason = format!("new binary '{failed}' failed health-check; reverted to '{prior}'");
        notify(
            notifier,
            "self-upgrade-reverted",
            Some(failed),
            Some(prior),
            &reason,
        )
        .await;
        ApplyOutcome::Reverted {
            failed: failed.to_owned(),
            restored: prior.to_owned(),
            reason,
        }
    } else {
        // Gate (c): the recovery of the recovery tool failed. Terminal — no
        // further automatic swap/restart, no false success.
        let reason = format!(
            "new binary '{failed}' failed health-check AND the revert target \
             '{prior}' is also unhealthy: manual intervention required"
        );
        notify(
            notifier,
            "self-upgrade-manual-intervention",
            Some(failed),
            Some(prior),
            &reason,
        )
        .await;
        ApplyOutcome::ManualIntervention { reason }
    }
}

/// Issue exactly one coordinated restart, then poll `is_active && healthy` until
/// both hold or the timeout elapses. Returns whether the unit came up healthy.
async fn bring_up(plan: &Plan, supervisor: &dyn Supervisor, health: &dyn Health) -> bool {
    if supervisor.restart().await.is_err() {
        return false;
    }
    tokio::time::timeout(plan.health_timeout, async {
        loop {
            if supervisor.is_active().await && health.healthy().await {
                return;
            }
            tokio::time::sleep(plan.poll_interval).await;
        }
    })
    .await
    .is_ok()
}

/// Build and deliver a [`FailurePayload`].
async fn notify(
    notifier: &dyn Notifier,
    event: &str,
    failed: Option<&str>,
    restored: Option<&str>,
    reason: &str,
) {
    notifier
        .notify(&FailurePayload {
            event: event.to_owned(),
            failed: failed.map(str::to_owned),
            restored: restored.map(str::to_owned),
            reason: reason.to_owned(),
        })
        .await;
}

/// The staged binary's id: the explicit version, else a short digest tag.
fn resolve_id(version: Option<&str>, digest: &str) -> String {
    version.map_or_else(
        || format!("sha-{}", &digest[..16.min(digest.len())]),
        ToOwned::to_owned,
    )
}

#[cfg(test)]
mod tests {
    use super::{apply, ApplyOutcome, Health, Plan, Supervisor};
    use crate::notify::{FailurePayload, Notifier};
    use crate::{Layout, Source};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    /// A fake unit whose liveness reflects whichever binary the `current` symlink
    /// points at: it reads the real symlink (as a real probe would observe the
    /// running server) and looks up that id's verdict. Unknown ids are healthy —
    /// so a "good" binary needs no entry; only the bad ones are listed.
    struct FakeUnit {
        layout: Layout,
        verdicts: HashMap<String, (bool, bool)>, // id -> (is_active, healthy)
        restarts: AtomicUsize,
    }

    impl FakeUnit {
        fn new(layout: Layout, bad: &[&str]) -> Self {
            let verdicts = bad
                .iter()
                .map(|id| ((*id).to_owned(), (false, false)))
                .collect();
            Self {
                layout,
                verdicts,
                restarts: AtomicUsize::new(0),
            }
        }

        fn verdict(&self) -> (bool, bool) {
            let active = self.layout.active().expect("read link");
            active
                .and_then(|id| self.verdicts.get(&id).copied())
                .unwrap_or((true, true))
        }
    }

    #[async_trait::async_trait]
    impl Supervisor for FakeUnit {
        async fn restart(&self) -> Result<(), String> {
            self.restarts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn is_active(&self) -> bool {
            self.verdict().0
        }
    }

    #[async_trait::async_trait]
    impl Health for FakeUnit {
        async fn healthy(&self) -> bool {
            self.verdict().1
        }
    }

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<FailurePayload>>,
    }

    #[async_trait::async_trait]
    impl Notifier for Recorder {
        async fn notify(&self, payload: &FailurePayload) {
            self.events.lock().unwrap().push(payload.clone());
        }
    }

    fn write_binary(dir: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join("incoming-fraisier");
        std::fs::write(&path, bytes).expect("write");
        path
    }

    fn plan(layout: Layout, source_path: std::path::PathBuf, version: &str) -> Plan {
        Plan {
            source: Source::Path {
                path: source_path,
                sha256: None,
            },
            layout,
            version: Some(version.to_owned()),
            keep: 2,
            systemd_available: true,
            health_timeout: Duration::from_millis(200),
            poll_interval: Duration::from_millis(2),
        }
    }

    #[tokio::test]
    async fn a_healthy_new_binary_commits_and_prunes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(dir.path().join("bin"));
        // A spare old + the live binary already staged & active.
        layout.stage("0.9.0", b"old-old").expect("stage 0.9.0");
        layout.stage("1.0.0", b"old").expect("stage 1.0.0");
        layout.activate("1.0.0").expect("activate 1.0.0");
        // Make 0.9.0 the oldest so a keep=2 prune reaps exactly it on commit.
        set_mtime(&layout.staged_path("0.9.0"), 100);
        set_mtime(&layout.staged_path("1.0.0"), 200);

        let new = write_binary(dir.path(), b"the healthy new binary");
        let fleet = FakeUnit::new(layout.clone(), &[]); // nothing bad
        let recorder = Recorder::default();

        let outcome = apply(
            &plan(layout.clone(), new, "2.0.0"),
            &fleet,
            &fleet,
            &recorder,
        )
        .await;

        assert_eq!(
            outcome,
            ApplyOutcome::Committed {
                id: "2.0.0".to_owned(),
                pruned: 1,
            }
        );
        assert_eq!(layout.active().unwrap().as_deref(), Some("2.0.0"));
        assert_eq!(fleet.restarts.load(Ordering::SeqCst), 1, "one restart");
        assert!(
            recorder.events.lock().unwrap().is_empty(),
            "no failure notify"
        );
        assert!(!layout.staged_path("0.9.0").exists(), "oldest pruned");
        assert!(layout.staged_path("1.0.0").exists(), "kept-old retained");
    }

    fn set_mtime(path: &std::path::Path, secs: u64) {
        let when = filetime::FileTime::from_unix_time(i64::try_from(secs).unwrap(), 0);
        filetime::set_file_mtime(path, when).expect("set mtime");
    }
}

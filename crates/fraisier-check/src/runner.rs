//! The concurrent check runner: [`run`] over a slice of [`Check`]s.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::Instant;

use crate::{Check, CheckOutcome, CheckRunReport, CheckStatus};

/// Run `checks` against `base_dir`, with at most `jobs` running concurrently.
///
/// A check passes iff its command exits `0`; a non-zero exit is
/// [`CheckStatus::Failed`] and a command that cannot be spawned is
/// [`CheckStatus::SpawnError`]. The run always completes and reports every check
/// — it never returns early on a failure. Results are in the order of `checks`
/// even though execution overlaps. `jobs` is clamped to at least `1`. Output is
/// captured, not streamed.
pub async fn run(checks: &[Check], jobs: usize, base_dir: &Path) -> CheckRunReport {
    let started = Instant::now();
    let lanes = jobs.max(1);
    let mut outcomes = Vec::with_capacity(checks.len());
    // Bounded concurrency by chunking into batches of `lanes` and awaiting each
    // batch together — the `RolloutStrategy::Rolling` pattern from multi_host.
    // `join_all` resolves positionally, so appending batches in order keeps
    // `outcomes` in config order.
    for batch in checks.chunks(lanes) {
        let pending = batch.iter().map(|check| run_one(check, base_dir));
        outcomes.extend(futures::future::join_all(pending).await);
    }
    CheckRunReport {
        outcomes,
        total_duration: started.elapsed(),
    }
}

/// Run a single check, mapping a spawn failure / exit code onto a [`CheckOutcome`].
async fn run_one(check: &Check, base_dir: &Path) -> CheckOutcome {
    let args = [OsString::from("-c"), OsString::from(check.command.as_str())];
    let cwd = check.workdir.as_deref().unwrap_or(base_dir);
    let started = Instant::now();
    let result = fraisier_adapter_support::run_command(
        OsStr::new("sh"),
        &args,
        &[],
        Some(cwd),
        "check",
        &check.name,
    )
    .await;
    let duration = started.elapsed();
    match result {
        Ok(captured) => {
            let status = if captured.succeeded() {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            };
            CheckOutcome {
                name: check.name.clone(),
                status,
                code: captured.code,
                stdout: captured.stdout,
                stderr: captured.stderr,
                duration,
            }
        }
        Err(error) => CheckOutcome {
            name: check.name.clone(),
            status: CheckStatus::SpawnError,
            code: None,
            stdout: String::new(),
            stderr: error.to_string(),
            duration,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::{Check, CheckStatus};
    use std::time::Duration;

    /// A check with no working directory.
    fn check(name: &str, command: &str) -> Check {
        Check {
            name: name.to_owned(),
            command: command.to_owned(),
            workdir: None,
        }
    }

    #[tokio::test]
    async fn a_failing_check_is_reported_failed_and_nonzero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = run(&[check("fail", "exit 3")], 1, dir.path()).await;
        assert!(!report.ok());
        assert_eq!(report.failed_count(), 1);
        assert_eq!(report.outcomes[0].status, CheckStatus::Failed);
        assert_eq!(report.outcomes[0].code, Some(3));
    }

    #[tokio::test]
    async fn a_spawn_failure_is_spawn_error_and_the_run_still_completes() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A non-existent working directory makes the spawn itself fail.
        let spec = Check {
            name: "spawn".to_owned(),
            command: "true".to_owned(),
            workdir: Some(dir.path().join("does-not-exist")),
        };
        let report = run(&[spec], 1, dir.path()).await;
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(report.outcomes[0].status, CheckStatus::SpawnError);
        assert_eq!(report.outcomes[0].code, None);
        assert!(!report.ok());
    }

    #[tokio::test]
    async fn a_passing_set_is_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = run(&[check("a", "true"), check("b", "exit 0")], 2, dir.path()).await;
        assert!(report.ok());
        assert_eq!(report.failed_count(), 0);
    }

    #[tokio::test]
    async fn results_preserve_config_order_despite_concurrency() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `a` sleeps and finishes after `b`, but must still be reported first.
        let report = run(
            &[check("a", "sleep 0.2"), check("b", "true")],
            2,
            dir.path(),
        )
        .await;
        assert_eq!(report.outcomes[0].name, "a");
        assert_eq!(report.outcomes[1].name, "b");
    }

    #[tokio::test]
    async fn parallelism_overlaps_with_two_jobs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = run(
            &[check("a", "sleep 0.3"), check("b", "sleep 0.3")],
            2,
            dir.path(),
        )
        .await;
        assert!(report.ok());
        let elapsed = report.total_duration;
        assert!(
            elapsed < Duration::from_millis(550),
            "two 300ms sleeps with jobs=2 should overlap, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn one_job_serializes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = run(
            &[check("a", "sleep 0.3"), check("b", "sleep 0.3")],
            1,
            dir.path(),
        )
        .await;
        let elapsed = report.total_duration;
        assert!(
            elapsed >= Duration::from_millis(550),
            "two 300ms sleeps with jobs=1 should serialize, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn jobs_zero_is_clamped_to_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = run(&[check("a", "true")], 0, dir.path()).await;
        assert!(report.ok());
        assert_eq!(report.outcomes.len(), 1);
    }

    #[tokio::test]
    async fn stdout_is_captured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = run(&[check("echo", "echo hello")], 1, dir.path()).await;
        assert!(report.outcomes[0].stdout.contains("hello"));
    }
}

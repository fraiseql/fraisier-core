//! The approval hook: an operator-supplied command that answers a
//! [`PolicyDecision::NeedsApproval`](fraisier_core::policy::PolicyDecision).
//!
//! [`ExecApproval`] is the one implementation. It is the seam where "a human or
//! an agent with authority signs this off" lives: fraisier does not authenticate
//! the approver, it runs the command the operator configured and believes its
//! exit code. Authority is the hook's business — a hook that shells out to an
//! on-call paging system, a chat approval, or an agent with a signed token all
//! plug in the same way.
//!
//! The design constraint that shapes everything here: **every way of not getting
//! a yes is a no.** A hook that cannot spawn, times out, exits non-zero, or
//! writes nothing intelligible is a refusal, never an approval and never an
//! error a caller might treat as "inconclusive, proceed" — the gate would
//! otherwise be theatre.

use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use fraisier_core::adapter_axes::RiskTier;
use fraisier_core::policy::{ApprovalHook, ApprovalRequest, ApprovalVerdict};

/// How much of the hook's stderr a refusal quotes.
///
/// Enough to name the cause, bounded so a hook that dumps a stack trace cannot
/// turn a saga failure message into a wall of text.
const REASON_LIMIT: usize = 200;

/// The `[policy].approval_command` hook: `sh -c <command>`, with the
/// [`ApprovalRequest`] as JSON on **stdin** and the deploy's identity in the
/// environment.
///
/// Never on argv — the same discipline that keeps a DSN out of a command line,
/// for the same reason: an argv value is world-readable in `ps` and can be
/// mistaken for a flag.
///
/// | Hook behaviour | Verdict |
/// |---|---|
/// | exits `0` | `Approved`, by the first non-empty stdout line |
/// | exits non-zero | `Denied`, quoting its stderr |
/// | still running at `timeout` | `Denied` (and the child is killed) |
/// | cannot be spawned | `Denied` |
#[derive(Debug, Clone)]
pub(crate) struct ExecApproval {
    /// The shell the command runs under. A field rather than a literal so the
    /// spawn-failure path is reachable from a test.
    shell: OsString,
    /// The operator's command, run as `sh -c <command>`.
    command: String,
    /// How long the hook may take before silence counts as refusal.
    timeout: Duration,
}

impl ExecApproval {
    /// A hook that runs `command` and gives up after `timeout`.
    pub(crate) fn new(command: impl Into<String>, timeout: Duration) -> Self {
        Self {
            shell: OsString::from("sh"),
            command: command.into(),
            timeout,
        }
    }

    /// Spawn the hook, write the request to its stdin, and capture its output.
    ///
    /// `Err` is a spawn or I/O failure — the hook never got to answer, which the
    /// caller turns into a refusal like every other way of not getting a yes.
    async fn run(&self, request: &ApprovalRequest) -> Result<std::process::Output, String> {
        // A request that cannot be serialized is a bug on our side, not the
        // hook's; send an empty object rather than failing the deploy for it.
        let payload = serde_json::to_string(request).unwrap_or_else(|_| "{}".to_owned());
        let mut command = tokio::process::Command::new(&self.shell);
        command
            .arg("-c")
            .arg(&self.command)
            .env("FRAISIER_APPROVAL_FRAISE", &request.fraise)
            .env("FRAISIER_APPROVAL_ENVIRONMENT", &request.environment)
            .env(
                "FRAISIER_APPROVAL_WORST_TIER",
                request.worst_tier.map_or("unclassified", RiskTier::as_str),
            )
            .env(
                "FRAISIER_APPROVAL_CHANGE_COUNT",
                request.reasons.len().to_string(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A hook that outlives its timeout is killed rather than left
            // running detached: the deploy has already refused, and a stray
            // approver process could still page someone.
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|error| format!("the approval hook could not be spawned: {error}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt as _;
            let _ = stdin.write_all(payload.as_bytes()).await;
            // Close stdin so a hook that reads to EOF (`cat`, `jq`) is not the
            // one thing that hangs the deploy.
            drop(stdin);
        }
        child
            .wait_with_output()
            .await
            .map_err(|error| format!("the approval hook could not be waited on: {error}"))
    }
}

/// The first non-empty, trimmed line of `text`, truncated to `limit` characters.
///
/// Character-wise, not byte-wise: a hook that writes UTF-8 must not be cut
/// mid-code-point.
fn first_line(text: &str, limit: usize) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut truncated: String = line.chars().take(limit).collect();
    if truncated.chars().count() < line.chars().count() {
        truncated.push('…');
    }
    Some(truncated)
}

#[async_trait]
impl ApprovalHook for ExecApproval {
    async fn request(&self, request: &ApprovalRequest) -> ApprovalVerdict {
        let output = match tokio::time::timeout(self.timeout, self.run(request)).await {
            Ok(Ok(output)) => output,
            Ok(Err(reason)) => return ApprovalVerdict::denied(reason),
            Err(_elapsed) => {
                return ApprovalVerdict::denied(format!(
                    "the approval hook did not answer within {}s",
                    self.timeout.as_secs_f32()
                ))
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        if output.status.success() {
            // An approver that says nothing is still an approver: name the hook
            // itself rather than recording an anonymous approval.
            return ApprovalVerdict::approved(
                first_line(&stdout, REASON_LIMIT)
                    .unwrap_or_else(|| format!("exec:{}", self.command)),
            );
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = first_line(&stderr, REASON_LIMIT)
            .or_else(|| first_line(&stdout, REASON_LIMIT))
            .unwrap_or_else(|| "no output".to_owned());
        ApprovalVerdict::denied(output.status.code().map_or_else(
            || format!("the approval hook was killed by a signal: {detail}"),
            |code| format!("the approval hook exited {code}: {detail}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{ExecApproval, REASON_LIMIT};
    use fraisier_core::adapter_axes::RiskTier;
    use fraisier_core::policy::{ApprovalHook, ApprovalRequest, ApprovalVerdict, PolicyReason};
    use std::ffi::OsString;
    use std::time::{Duration, Instant};

    /// A request to sign off on dropping one column.
    fn request() -> ApprovalRequest {
        ApprovalRequest::new(
            "checkout",
            "production",
            vec![PolicyReason::new(
                Some(RiskTier::Irreversible),
                "public.tb_user.legacy_flag",
                "drop_column",
                Some("20260804120100_drop_legacy".to_owned()),
            )],
        )
    }

    /// A hook running `command` with a generous timeout.
    fn hook(command: &str) -> ExecApproval {
        ExecApproval::new(command, Duration::from_secs(30))
    }

    /// The refusal reason, or a panic naming what was answered instead.
    fn denial(verdict: &ApprovalVerdict) -> &str {
        match verdict {
            ApprovalVerdict::Denied { reason } => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_denied() {
        let verdict = hook("exit 3").request(&request()).await;
        assert!(denial(&verdict).contains('3'), "{verdict:?}");
    }

    #[tokio::test]
    async fn a_hook_that_cannot_be_executed_is_denied() {
        // The failure an operator actually hits: a typo in `approval_command`.
        let verdict = hook("/nonexistent/approve.sh").request(&request()).await;
        assert!(
            matches!(verdict, ApprovalVerdict::Denied { .. }),
            "{verdict:?}"
        );
    }

    #[tokio::test]
    async fn a_spawn_failure_is_denied() {
        // No shell to run the hook under at all — the one path that never
        // reaches an exit code.
        let hook = ExecApproval {
            shell: OsString::from("/nonexistent/sh"),
            command: "true".to_owned(),
            timeout: Duration::from_secs(30),
        };
        assert!(denial(&hook.request(&request()).await).contains("could not be spawned"));
    }

    #[tokio::test]
    async fn a_timeout_is_denied() {
        let hook = ExecApproval::new("sleep 30", Duration::from_millis(200));
        assert!(denial(&hook.request(&request()).await).contains("did not answer"));
    }

    #[tokio::test]
    async fn a_hook_that_hangs_does_not_hang_the_deploy() {
        // The timeout is a real bound, not a message: an unattended deploy must
        // refuse and move on rather than wait for an approver who never answers.
        let hook = ExecApproval::new("sleep 30", Duration::from_millis(200));
        let started = Instant::now();
        let verdict = hook.request(&request()).await;
        assert!(
            matches!(verdict, ApprovalVerdict::Denied { .. }),
            "{verdict:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the call returned after {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn stderr_is_truncated_into_the_reason() {
        let noise = "x".repeat(REASON_LIMIT * 2);
        let verdict = hook(&format!("echo '{noise}' >&2; exit 1"))
            .request(&request())
            .await;
        let reason = denial(&verdict);
        assert!(reason.contains('…'), "{reason}");
        assert!(reason.chars().count() < noise.chars().count(), "{reason}");
    }

    #[tokio::test]
    async fn the_request_reaches_the_hook_on_stdin() {
        // Not argv: an approval payload on a command line is world-readable in
        // `ps` and can be mistaken for a flag.
        let dir = tempfile::tempdir().expect("tempdir");
        let seen = dir.path().join("stdin.json");
        let verdict = hook(&format!("cat > {}; exit 1", seen.display()))
            .request(&request())
            .await;
        assert!(
            matches!(verdict, ApprovalVerdict::Denied { .. }),
            "{verdict:?}"
        );
        let payload = std::fs::read_to_string(&seen).expect("the hook read its stdin");
        assert!(payload.contains("public.tb_user.legacy_flag"), "{payload}");
        assert!(payload.contains("irreversible"), "{payload}");
        assert!(payload.contains("20260804120100_drop_legacy"), "{payload}");
    }

    #[tokio::test]
    async fn the_environment_carries_the_context_vars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seen = dir.path().join("env");
        let verdict = hook(&format!("env > {}; exit 1", seen.display()))
            .request(&request())
            .await;
        assert!(
            matches!(verdict, ApprovalVerdict::Denied { .. }),
            "{verdict:?}"
        );
        let env = std::fs::read_to_string(&seen).expect("the hook ran");
        for expected in [
            "FRAISIER_APPROVAL_FRAISE=checkout",
            "FRAISIER_APPROVAL_ENVIRONMENT=production",
            "FRAISIER_APPROVAL_WORST_TIER=irreversible",
            "FRAISIER_APPROVAL_CHANGE_COUNT=1",
        ] {
            assert!(
                env.lines().any(|line| line == expected),
                "missing {expected}"
            );
        }
    }

    #[tokio::test]
    async fn exit_zero_is_approved() {
        let verdict = hook("true").request(&request()).await;
        assert!(
            matches!(verdict, ApprovalVerdict::Approved { .. }),
            "{verdict:?}"
        );
    }

    #[tokio::test]
    async fn the_approver_comes_from_stdout() {
        // Who signed off is the audit record's whole value; a hook that names
        // its approver must have that name survive into the decision.
        let verdict = hook("echo 'oncall@example.com'").request(&request()).await;
        assert_eq!(
            verdict,
            ApprovalVerdict::approved("oncall@example.com"),
            "{verdict:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_stdout_falls_back_to_the_command_name() {
        // A silent approver is still an identifiable one: record the hook rather
        // than an anonymous approval.
        let verdict = hook("exit 0").request(&request()).await;
        assert_eq!(verdict, ApprovalVerdict::approved("exec:exit 0"));
    }

    #[test]
    fn the_hook_is_told_the_identity_and_nothing_else() {
        // The hook receives the deploy's identity and the tiers — never the
        // `AdapterCtx`, so no DSN, secret name, or adapter setting can reach it.
        // Asserting the exported set is *exactly* these four is what keeps a
        // later "just pass the context through" from being a silent leak. (The
        // hook still inherits the operator's own environment, as every fraisier
        // hook does; what fraisier itself adds is this and only this.)
        //
        // Synchronous, under the shared lock: the child inherits the *process*
        // environment, so a db-op test mid-`set_var` on another thread would
        // otherwise show up here as a variable fraisier never exported.
        let _guard = crate::test_env::ENV_LOCK.lock().expect("env lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let seen = dir.path().join("env");
        let _ = crate::test_env::block_on(
            hook(&format!("env > {}; exit 1", seen.display())).request(&request()),
        );
        let env = std::fs::read_to_string(&seen).expect("the hook ran");
        let mut exported: Vec<&str> = env
            .lines()
            .filter(|line| line.starts_with("FRAISIER_"))
            .filter_map(|line| line.split('=').next())
            .collect();
        exported.sort_unstable();
        assert_eq!(
            exported,
            [
                "FRAISIER_APPROVAL_CHANGE_COUNT",
                "FRAISIER_APPROVAL_ENVIRONMENT",
                "FRAISIER_APPROVAL_FRAISE",
                "FRAISIER_APPROVAL_WORST_TIER",
            ]
        );
    }
}

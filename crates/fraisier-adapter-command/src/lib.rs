//! # fraisier-adapter-command
//!
//! The **universal escape-hatch** migration adapter (PRD §6.3): a
//! [`MigrationAdapter`] that runs user-configured shell commands. It lets any
//! migration tool fraisier does not natively wrap be driven through the same
//! frozen trait — "if you can run it from a shell, you can deploy it".
//!
//! This crate also provides [`CommandHealth`], a sibling [`HealthAdapter`] that
//! runs a configured shell command as the post-deploy health gate (exit 0 →
//! healthy; any non-zero exit → unhealthy; spawn failure or timeout → error). It
//! reuses the same `sh -c`/argv command shape and secret-env handling.
//!
//! ## Configuration
//!
//! The adapter is built from its `[migration.command]` settings table (via
//! [`CommandMigration::from_settings`]); commands live under `commands`. Each
//! entry is either a shell string (run via `sh -c`) or an argv array (run
//! directly, no shell):
//!
//! ```toml
//! [migration.command.commands]
//! current_revision = "mytool current --quiet"
//! up = "mytool migrate up"
//! down_to = ["mytool", "migrate", "down"]
//! verify = "mytool check"
//! ```
//!
//! - `current_revision` prints the current revision on stdout (empty = none).
//! - `up` / `down_to` apply / roll back; a non-zero exit is a failure.
//! - `verify` exits 0 when correct, non-zero when a check fails.
//!
//! Because [`describe`](MigrationAdapter::describe) advertises only configured
//! commands and takes no context, the command set is fixed at construction.
//!
//! ## Working directory and the release context
//!
//! The deploy runs these commands with their working directory set to the staged
//! release (see [`AdapterCtx::workdir`]), so a relative `up = "bash
//! scripts/deploy/prepare.sh"` resolves against the release it was cut from. Each
//! command also receives the release context in its environment —
//! `FRAISIER_RELEASE_DIR` (the working directory), and, when configured,
//! `FRAISIER_ACTIVE_PATH` and `FRAISIER_APP_VERSION` — so a source-run prepare
//! script can reference the deploy's paths without coupling to a fixed version.
//!
//! ## Secrets and the target revision (never in argv)
//!
//! Every declared secret in [`AdapterCtx::env_secrets`] is resolved via
//! [`AdapterCtx::secret`] and exported to the command's environment under its
//! logical name (so a command can read `$DATABASE_URL`). The target revision for
//! `up`/`down_to` is exported as `FRAISIER_TARGET`. Neither secrets nor the
//! target are ever placed in argv — consistent with Decision 5.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::time::Duration;

use async_trait::async_trait;
use fraisier_adapter_support::{error, run_command, Captured};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, HealthAdapter, HealthStatus,
    HostId, MigrationAdapter, MigrationOutcome, Revision, VerifyCheck, VerifyReport,
};
use serde_json::Value;

/// The adapter's default identity name.
const DEFAULT_NAME: &str = "command";

/// The IPC protocol major version this adapter's contract matches.
const PROTOCOL_VERSION: u32 = 1;

/// The env var the target revision is exported under for `up`/`down_to`.
const TARGET_ENV: &str = "FRAISIER_TARGET";

/// The env var carrying the migration command's working directory — the staged
/// release for a release-based deploy.
const RELEASE_DIR_ENV: &str = "FRAISIER_RELEASE_DIR";

/// The env var carrying the `active_path` symlink target, when configured.
const ACTIVE_PATH_ENV: &str = "FRAISIER_ACTIVE_PATH";

/// The env var carrying the app version being deployed, when known.
const APP_VERSION_ENV: &str = "FRAISIER_APP_VERSION";

/// Method keys recognised under `settings.commands`, in capability order.
const METHOD_KEYS: &[&str] = &["current_revision", "up", "down_to", "verify"];

/// A configured command: either a shell string or a direct argv vector.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandSpec {
    /// Run `sh -c "<string>"`.
    Shell(String),
    /// Run the argv directly, no shell.
    Argv(Vec<String>),
}

impl CommandSpec {
    /// Parse a spec from a settings value (`"cmd"` or `["cmd", "arg"]`).
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(shell) => Some(Self::Shell(shell.clone())),
            Value::Array(items) => {
                let argv: Vec<String> = items
                    .iter()
                    .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                    .collect();
                (argv.len() == items.len() && !argv.is_empty()).then_some(Self::Argv(argv))
            }
            _ => None,
        }
    }

    /// The program and arguments to spawn.
    fn program_and_args(&self) -> (OsString, Vec<OsString>) {
        match self {
            Self::Shell(shell) => (
                OsString::from("sh"),
                vec![OsString::from("-c"), OsString::from(shell)],
            ),
            Self::Argv(argv) => {
                let mut iter = argv.iter().map(OsString::from);
                let program = iter.next().unwrap_or_default();
                (program, iter.collect())
            }
        }
    }
}

/// The universal command-driven migration adapter.
///
/// # Example
/// ```
/// use std::collections::BTreeMap;
/// use fraisier_adapter_command::CommandMigration;
///
/// let mut settings = BTreeMap::new();
/// settings.insert(
///     "commands".to_owned(),
///     serde_json::json!({ "up": "mytool up", "current_revision": "mytool current" }),
/// );
/// let adapter = CommandMigration::from_settings("command", &settings);
/// let _ = adapter;
/// ```
pub struct CommandMigration {
    name: String,
    commands: BTreeMap<String, CommandSpec>,
}

impl Default for CommandMigration {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandMigration {
    /// Create an adapter identified as `"command"` with no commands configured.
    #[must_use]
    pub fn new() -> Self {
        Self {
            name: DEFAULT_NAME.to_owned(),
            commands: BTreeMap::new(),
        }
    }

    /// Build an adapter named `name` from a `[migration.<name>]` settings table,
    /// reading recognised commands from its `commands` sub-table. Unrecognised or
    /// malformed entries are ignored.
    #[must_use]
    pub fn from_settings(name: impl Into<String>, settings: &BTreeMap<String, Value>) -> Self {
        let commands = settings
            .get("commands")
            .and_then(Value::as_object)
            .map(|table| {
                METHOD_KEYS
                    .iter()
                    .filter_map(|&method| {
                        table
                            .get(method)
                            .and_then(CommandSpec::from_value)
                            .map(|spec| (method.to_owned(), spec))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            name: name.into(),
            commands,
        }
    }

    /// Run the command configured for `method`, with `extra_env` exported on top
    /// of the resolved secrets.
    async fn run_method(
        &self,
        method: &str,
        ctx: &AdapterCtx,
        extra_env: Vec<(OsString, OsString)>,
    ) -> Result<Captured, AdapterError> {
        let spec = self.commands.get(method).ok_or_else(|| {
            error(
                AdapterErrorKind::InvalidConfig,
                &self.name,
                method,
                format!(
                    "no '{method}' command configured for adapter '{}'",
                    self.name
                ),
                None,
            )
        })?;

        let mut envs = resolve_secret_env(ctx, &self.name)?;
        envs.extend(release_env(ctx));
        envs.extend(extra_env);
        let (program, args) = spec.program_and_args();
        run_command(
            &program,
            &args,
            &envs,
            Some(ctx.workdir.as_path()),
            &self.name,
            method,
        )
        .await
    }

    /// Build a failure error from a non-zero `captured` exit.
    fn failure(&self, operation: &str, captured: &Captured) -> AdapterError {
        let code = captured
            .code
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        error(
            AdapterErrorKind::Execution,
            &self.name,
            operation,
            format!("'{operation}' command exited with {code}"),
            captured.stderr_opt(),
        )
    }
}

/// Resolve every declared secret to a `(logical_name, value)` env pair.
fn resolve_secret_env(
    ctx: &AdapterCtx,
    adapter: &str,
) -> Result<Vec<(OsString, OsString)>, AdapterError> {
    ctx.env_secrets
        .keys()
        .map(|logical| {
            let value = ctx
                .secret(logical)
                .map_err(|err| err.with_adapter(adapter))?;
            Ok((OsString::from(logical), OsString::from(value)))
        })
        .collect()
}

/// The release-context env vars exported to every migration command, so a
/// source-run prepare script can reference the deploy's paths **portably** —
/// without hard-coding an absolute release path coupled to `--app-version`:
///
/// - `FRAISIER_RELEASE_DIR` — the command's working directory, which the deploy
///   sets to the staged release for a release-based deploy (so relative paths and
///   this var both point at the freshly-cut release).
/// - `FRAISIER_ACTIVE_PATH` — the `active_path` symlink swapped in on activate,
///   when configured.
/// - `FRAISIER_APP_VERSION` — the app version being deployed, when known.
///
/// The last two are omitted when their `[artifact]` settings are absent, so a
/// script can distinguish "not configured" from an empty value.
fn release_env(ctx: &AdapterCtx) -> Vec<(OsString, OsString)> {
    let mut envs = vec![(
        OsString::from(RELEASE_DIR_ENV),
        ctx.workdir.clone().into_os_string(),
    )];
    if let Some(active_path) = ctx.settings.get("active_path").and_then(Value::as_str) {
        envs.push((OsString::from(ACTIVE_PATH_ENV), OsString::from(active_path)));
    }
    if let Some(version) = ctx.settings.get("version").and_then(Value::as_str) {
        envs.push((OsString::from(APP_VERSION_ENV), OsString::from(version)));
    }
    envs
}

#[async_trait]
impl MigrationAdapter for CommandMigration {
    async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
        // Advertise only configured commands, in canonical order.
        let capabilities = METHOD_KEYS
            .iter()
            .filter(|method| self.commands.contains_key(**method))
            .map(|method| (*method).to_owned())
            .collect();
        Ok(AdapterDescription {
            name: self.name.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: PROTOCOL_VERSION,
            capabilities,
        })
    }

    async fn current_revision(&self, ctx: &AdapterCtx) -> Result<Option<Revision>, AdapterError> {
        let captured = self.run_method("current_revision", ctx, Vec::new()).await?;
        if !captured.succeeded() {
            return Err(self.failure("current_revision", &captured));
        }
        let revision = captured.stdout.trim();
        Ok((!revision.is_empty()).then(|| Revision::new(revision)))
    }

    async fn up(
        &self,
        ctx: &AdapterCtx,
        target: Option<Revision>,
    ) -> Result<MigrationOutcome, AdapterError> {
        let extra = target
            .as_ref()
            .map(|rev| vec![(OsString::from(TARGET_ENV), OsString::from(rev.as_str()))])
            .unwrap_or_default();
        let captured = self.run_method("up", ctx, extra).await?;
        if !captured.succeeded() {
            return Err(self.failure("up", &captured));
        }
        Ok(MigrationOutcome {
            from: None,
            to: target,
            applied: Vec::new(),
            log: captured.stdout,
        })
    }

    async fn down_to(
        &self,
        ctx: &AdapterCtx,
        target: Revision,
    ) -> Result<MigrationOutcome, AdapterError> {
        let extra = vec![(OsString::from(TARGET_ENV), OsString::from(target.as_str()))];
        let captured = self.run_method("down_to", ctx, extra).await?;
        if !captured.succeeded() {
            return Err(self.failure("down_to", &captured));
        }
        Ok(MigrationOutcome {
            from: None,
            to: Some(target),
            applied: Vec::new(),
            log: captured.stdout,
        })
    }

    async fn verify(&self, ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
        // No verify command configured ⇒ nothing to check (vacuously ok).
        if !self.commands.contains_key("verify") {
            return Ok(VerifyReport {
                ok: true,
                checks: Vec::new(),
            });
        }
        let captured = self.run_method("verify", ctx, Vec::new()).await?;
        let ok = captured.succeeded();
        // A failed check is a *result* (ok:false), not an adapter error.
        let detail = if ok {
            captured.stdout.trim().to_owned()
        } else {
            captured
                .stderr_opt()
                .unwrap_or_else(|| captured.stdout.trim().to_owned())
        };
        Ok(VerifyReport {
            ok,
            checks: vec![VerifyCheck {
                name: "command verify".to_owned(),
                ok,
                detail: (!detail.is_empty()).then_some(detail),
            }],
        })
    }
}

/// The health-axis identity name for the command adapter.
const HEALTH_NAME: &str = "command";

/// Default fail-closed timeout for a command health probe, in milliseconds.
/// Generous on purpose: a perf/regression scan can legitimately take tens of
/// seconds, and a premature timeout would roll back a healthy deploy.
const DEFAULT_HEALTH_TIMEOUT_MS: u64 = 60_000;

/// A [`HealthAdapter`] that runs a configured shell command as the saga health gate.
///
/// Exit `0` → healthy; any non-zero exit → unhealthy (with a captured detail
/// excerpt); a spawn failure or a timeout → [`AdapterError`] (the probe could not
/// be performed). Both the unhealthy and error paths roll the deploy back, so the
/// gate fails closed.
///
/// # Configuration
/// Read per call from [`AdapterCtx::settings`] (the `[health]` table), mirroring
/// the HTTP adapter:
/// - `command` — a shell string (run via `sh -c`) or an argv array (no shell).
///   Required.
/// - `timeout_ms` — the fail-closed timeout (default 60000).
///
/// Secrets (including the DSN) reach the command via the environment, never
/// argv: every entry in [`AdapterCtx::env_secrets`] is exported under its logical
/// name, exactly as for the migration command adapter.
///
/// # Example
/// ```
/// use fraisier_adapter_command::CommandHealth;
///
/// let adapter = CommandHealth::new();
/// let _ = adapter;
/// ```
#[derive(Default)]
pub struct CommandHealth {
    _private: (),
}

impl CommandHealth {
    /// Create a command health adapter. Its configuration is read from the
    /// [`AdapterCtx`] at probe time, so the constructor takes no arguments.
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

/// Format a one-line, greppable detail from a perf `regression-scan --json`
/// report on `stdout`, naming the top regressed `(object_type, modification_type)`
/// with its p50 delta and a count of any others
/// (e.g. `perf regression: order/UPDATE p50 +42% (12ms→17ms), 3 more`).
///
/// Returns `None` — so the caller falls back to the plain excerpt — when stdout
/// is not the expected shape: human-format output, a different tool, malformed
/// JSON, or an empty `findings` list. Parsing is total and defensive: a contract
/// drift degrades gracefully, never panicking the gate.
///
/// Targets the fraiseql v2.6.0 `RegressionReport` contract (FraiseQL #392).
fn format_perf_detail(stdout: &str) -> Option<String> {
    let report: Value = serde_json::from_str(stdout.trim()).ok()?;
    let findings = report.get("findings")?.as_array()?;
    let first = findings.first()?;
    let object_type = first.get("object_type")?.as_str()?;
    let modification_type = first.get("modification_type")?.as_str()?;
    let pct_change = first.get("pct_change")?.as_f64()?;
    let baseline_p50 = first.get("baseline_p50")?.as_f64()?;
    let recent_p50 = first.get("recent_p50")?.as_f64()?;
    let more = findings.len() - 1;
    let suffix = if more > 0 {
        format!(", {more} more")
    } else {
        String::new()
    };
    Some(format!(
        "perf regression: {object_type}/{modification_type} \
         p50 {pct_change:+.0}% ({baseline_p50:.0}ms→{recent_p50:.0}ms){suffix}"
    ))
}

#[async_trait]
impl HealthAdapter for CommandHealth {
    async fn check(&self, ctx: &AdapterCtx, _host: &HostId) -> Result<HealthStatus, AdapterError> {
        let spec = ctx
            .settings
            .get("command")
            .and_then(CommandSpec::from_value)
            .ok_or_else(|| {
                error(
                    AdapterErrorKind::InvalidConfig,
                    HEALTH_NAME,
                    "check",
                    "no 'command' configured in [health] settings".to_owned(),
                    None,
                )
            })?;
        let timeout = Duration::from_millis(
            ctx.settings
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_HEALTH_TIMEOUT_MS),
        );
        let envs = resolve_secret_env(ctx, HEALTH_NAME)?;
        let (program, args) = spec.program_and_args();

        let run = run_command(
            &program,
            &args,
            &envs,
            Some(ctx.workdir.as_path()),
            HEALTH_NAME,
            "check",
        );
        let captured = match tokio::time::timeout(timeout, run).await {
            Ok(result) => result?,
            Err(_elapsed) => {
                return Err(error(
                    AdapterErrorKind::Execution,
                    HEALTH_NAME,
                    "check",
                    format!("health command timed out after {}ms", timeout.as_millis()),
                    None,
                ));
            }
        };

        if captured.succeeded() {
            return Ok(HealthStatus {
                healthy: true,
                detail: None,
            });
        }
        // A non-zero exit is a *result* (unhealthy), not an adapter error — the
        // command ran. Prefer a structured, named detail parsed from the scan's
        // `--json` stdout; fall back to a trimmed stderr (else stdout) excerpt for
        // human output or any other command.
        let detail = format_perf_detail(&captured.stdout).or_else(|| {
            let stderr = captured.stderr.trim();
            let excerpt = if stderr.is_empty() {
                captured.stdout.trim()
            } else {
                stderr
            };
            (!excerpt.is_empty()).then(|| excerpt.to_owned())
        });
        Ok(HealthStatus {
            healthy: false,
            detail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_perf_detail, resolve_secret_env, CommandHealth, CommandMigration, CommandSpec,
    };
    use fraisier_core::adapter_axes::{AdapterCtx, HealthAdapter, HostId, MigrationAdapter};
    use serde_json::{json, Value};
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    /// A perf regression-scan `--json` report carrying `findings` (the pinned
    /// fraiseql v2.6.0 shape).
    fn report(findings: &Value) -> String {
        json!({
            "findings": findings.clone(),
            "skipped": [],
            "summary": { "groups_analyzed": 2, "regressions": 1, "total_samples": 200, "excluded_samples": 0 },
        })
        .to_string()
    }

    fn order_update_finding() -> Value {
        json!({
            "object_type": "order",
            "modification_type": "UPDATE",
            "baseline_p50": 12.0,
            "baseline_p95": 20.0,
            "recent_p50": 17.0,
            "recent_p95": 28.0,
            "pct_change": 41.67,
            "baseline_samples": 120,
            "recent_samples": 140,
        })
    }

    #[test]
    fn perf_detail_names_the_single_finding() {
        let detail = format_perf_detail(&report(&json!([order_update_finding()])))
            .expect("a findings report formats");
        assert_eq!(detail, "perf regression: order/UPDATE p50 +42% (12ms→17ms)");
    }

    #[test]
    fn perf_detail_counts_additional_findings() {
        let second = json!({
            "object_type": "invoice",
            "modification_type": "INSERT",
            "baseline_p50": 5.0,
            "baseline_p95": 9.0,
            "recent_p50": 8.0,
            "recent_p95": 14.0,
            "pct_change": 60.0,
            "baseline_samples": 80,
            "recent_samples": 90,
        });
        let detail = format_perf_detail(&report(&json!([order_update_finding(), second])))
            .expect("a findings report formats");
        assert_eq!(
            detail,
            "perf regression: order/UPDATE p50 +42% (12ms→17ms), 1 more"
        );
    }

    #[test]
    fn perf_detail_degrades_on_non_report_output() {
        // Human-format output, malformed JSON, an empty findings list, and a
        // wrong shape must all degrade to None so the caller falls back.
        assert!(format_perf_detail("WARN order/UPDATE p50 +42%").is_none());
        assert!(format_perf_detail("{not json").is_none());
        assert!(format_perf_detail(&report(&json!([]))).is_none());
        assert!(format_perf_detail(&json!({"other": 1}).to_string()).is_none());
    }

    /// An `AdapterCtx` whose `[health]` settings carry `command`.
    fn health_ctx(command: Value) -> AdapterCtx {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.settings.insert("command".to_owned(), command);
        ctx
    }

    #[tokio::test]
    async fn command_health_exit_zero_is_healthy() {
        let status = CommandHealth::new()
            .check(&health_ctx(json!("true")), &HostId::new("localhost"))
            .await
            .expect("check runs");
        assert!(status.healthy);
        assert!(status.detail.is_none());
    }

    #[tokio::test]
    async fn command_health_nonzero_is_unhealthy_with_stderr_detail() {
        let status = CommandHealth::new()
            .check(
                &health_ctx(json!("echo boom >&2; exit 1")),
                &HostId::new("localhost"),
            )
            .await
            .expect("check runs");
        assert!(!status.healthy);
        assert_eq!(status.detail.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn command_health_nonzero_falls_back_to_stdout_detail() {
        let status = CommandHealth::new()
            .check(
                &health_ctx(json!("echo from-stdout; exit 2")),
                &HostId::new("localhost"),
            )
            .await
            .expect("check runs");
        assert!(!status.healthy);
        assert_eq!(status.detail.as_deref(), Some("from-stdout"));
    }

    #[tokio::test]
    async fn command_health_spawn_failure_is_adapter_error() {
        // Argv form with a missing binary → a real spawn failure (the program is
        // the binary itself, not `sh`), distinct from a non-zero exit.
        let err = CommandHealth::new()
            .check(
                &health_ctx(json!(["/definitely/not/a/real/binary-xyz"])),
                &HostId::new("localhost"),
            )
            .await
            .expect_err("spawn fails");
        assert_eq!(err.adapter.as_deref(), Some("command"));
    }

    #[tokio::test]
    async fn command_health_timeout_is_adapter_error() {
        let mut ctx = health_ctx(json!("sleep 5"));
        ctx.settings.insert("timeout_ms".to_owned(), json!(50));
        let err = CommandHealth::new()
            .check(&ctx, &HostId::new("localhost"))
            .await
            .expect_err("times out");
        assert_eq!(err.adapter.as_deref(), Some("command"));
    }

    #[tokio::test]
    async fn command_health_requires_a_command() {
        let err = CommandHealth::new()
            .check(
                &AdapterCtx::new("checkout", "production"),
                &HostId::new("localhost"),
            )
            .await
            .expect_err("missing command");
        assert_eq!(err.adapter.as_deref(), Some("command"));
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn settings_with(commands: Value) -> BTreeMap<String, Value> {
        let mut settings = BTreeMap::new();
        settings.insert("commands".to_owned(), commands);
        settings
    }

    #[test]
    fn command_spec_parses_shell_and_argv() {
        assert_eq!(
            CommandSpec::from_value(&json!("mytool up")),
            Some(CommandSpec::Shell("mytool up".to_owned()))
        );
        assert_eq!(
            CommandSpec::from_value(&json!(["mytool", "up"])),
            Some(CommandSpec::Argv(vec![
                "mytool".to_owned(),
                "up".to_owned()
            ]))
        );
        assert_eq!(CommandSpec::from_value(&json!([])), None);
        assert_eq!(CommandSpec::from_value(&json!(42)), None);
    }

    #[test]
    fn program_and_args_for_each_form() {
        let (prog, args) = CommandSpec::Shell("echo hi".to_owned()).program_and_args();
        assert_eq!(prog, OsString::from("sh"));
        assert_eq!(args, vec![OsString::from("-c"), OsString::from("echo hi")]);

        let (prog, args) =
            CommandSpec::Argv(vec!["mytool".to_owned(), "up".to_owned()]).program_and_args();
        assert_eq!(prog, OsString::from("mytool"));
        assert_eq!(args, vec![OsString::from("up")]);
    }

    #[tokio::test]
    async fn describe_advertises_only_configured_commands_in_order() {
        let settings = settings_with(json!({ "verify": "v", "up": "u", "current_revision": "c" }));
        let adapter = CommandMigration::from_settings("command", &settings);
        let desc = adapter.describe().await.expect("describe");
        assert_eq!(desc.name, "command");
        // Canonical order: current_revision, up, down_to, verify — down_to absent.
        assert_eq!(desc.capabilities, vec!["current_revision", "up", "verify"]);
    }

    #[test]
    fn from_settings_ignores_malformed_and_unknown() {
        let settings = settings_with(json!({ "up": 42, "bogus": "x", "verify": "ok" }));
        let adapter = CommandMigration::from_settings("command", &settings);
        assert!(!adapter.commands.contains_key("up")); // malformed (number) dropped
        assert!(!adapter.commands.contains_key("bogus")); // unknown key dropped
        assert!(adapter.commands.contains_key("verify"));
    }

    #[test]
    fn resolve_secret_env_reads_through_mapping() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let source = "FRAISIER_CMD_TEST_SECRET";
        std::env::set_var(source, "postgres://example/db");
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.env_secrets
            .insert("DATABASE_URL".to_owned(), source.to_owned());

        let envs = resolve_secret_env(&ctx, "command").expect("resolve");
        std::env::remove_var(source);

        assert_eq!(
            envs,
            vec![(
                OsString::from("DATABASE_URL"),
                OsString::from("postgres://example/db")
            )]
        );
    }

    #[test]
    fn resolve_secret_env_fails_when_source_unset() {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.env_secrets.insert(
            "DATABASE_URL".to_owned(),
            "FRAISIER_CMD_DEFINITELY_UNSET_VAR".to_owned(),
        );
        let err = resolve_secret_env(&ctx, "command").expect_err("unset source must fail");
        assert_eq!(err.adapter.as_deref(), Some("command"));
    }
}

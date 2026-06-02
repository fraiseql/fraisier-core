//! # fraisier-adapter-confiture
//!
//! The in-process [`ConfitureMigration`] adapter: a [`MigrationAdapter`] that
//! wraps the [Confiture](https://pypi.org/project/fraiseql-confiture/) migration
//! CLI (`confiture migrate <subcommand>`). It is the native, intimate-integration
//! migration adapter of fraisier (PRD §6.3) — not an IPC subprocess adapter, but
//! still implementing the *same* frozen trait.
//!
//! ## DSN handoff — secrets via environment, never argv
//!
//! The adapter resolves the database DSN through [`AdapterCtx::secret`] (logical
//! name `"DATABASE_URL"`) and hands it to Confiture by setting
//! **`CONFITURE_DATABASE_URL`** on the child process and passing **`--no-config`**.
//! Confiture 0.20.0's `--no-config` makes the environment the *sole* DSN source,
//! so a stray `db/environments/*.yaml` in the deploy workdir can never shadow the
//! operator's DSN (the #152 precedence contract). The DSN never appears in argv —
//! honouring PRD review Decision 5 and the convergence rule that the in-process
//! and IPC paths handle secrets identically.
//!
//! ## Connection requirements
//!
//! Confiture ≥ 0.20.0 is required: it provides `migrate current`, `migrate
//! down-to`, and the `--no-config` env-only DSN mode this adapter depends on.
//!
//! ## Double locking (intentional)
//!
//! Confiture takes its own DB-level migration lock; the saga takes a deploy-level
//! lock via the `StateStore`. Both layers stay (PRD review §3): the `StateStore`
//! lock serialises the *deploy*, Confiture's lock serialises the *database
//! migration* against any other source. The adapter never passes `--no-lock` or
//! `--force`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, MigrationAdapter,
    MigrationOutcome, PreflightIssue, PreflightReport, Revision, Severity, VerifyCheck,
    VerifyReport,
};
use serde_json::Value;

/// The adapter's discovery/identity name.
const ADAPTER_NAME: &str = "confiture";

/// The IPC protocol major version this adapter's contract matches.
const PROTOCOL_VERSION: u32 = 1;

/// The logical secret name the adapter resolves from [`AdapterCtx::env_secrets`].
const DATABASE_URL_LOGICAL: &str = "DATABASE_URL";

/// The env var Confiture reads for its tracking-database DSN under `--no-config`.
const CONFITURE_DSN_ENV: &str = "CONFITURE_DATABASE_URL";

/// The env var that overrides which `confiture` binary the adapter spawns.
const PROGRAM_ENV: &str = "FRAISIER_CONFITURE_BIN";

/// The methods this adapter genuinely implements, advertised via [`describe`].
///
/// `post_migrate` is intentionally absent: Confiture has no post-migrate
/// subcommand, so the adapter keeps the trait's safe no-op default rather than
/// advertise a capability it cannot meaningfully fulfil (PRD review Decision 3).
///
/// [`describe`]: MigrationAdapter::describe
const CAPABILITIES: &[&str] = &["current_revision", "up", "down_to", "verify", "preflight"];

/// Confiture's error code for a reachable-but-uninitialised database (tracking
/// table absent). `migrate current` reports it with exit code 2; the adapter
/// maps it to "no current revision" rather than an error.
const UNINITIALISED_ERROR_CODE: &str = "PRECON_1001";

/// Process-wide counter making each `--output` temp file path unique.
static OUTPUT_SEQ: AtomicU64 = AtomicU64::new(0);

/// The in-process Confiture migration adapter.
///
/// Construct with [`ConfitureMigration::new`] (which honours the
/// `FRAISIER_CONFITURE_BIN` override) and use it anywhere a
/// [`MigrationAdapter`] is expected.
///
/// # Example
/// ```
/// use fraisier_adapter_confiture::ConfitureMigration;
///
/// let adapter = ConfitureMigration::new();
/// // Point at a specific binary (e.g. in tests):
/// let pinned = ConfitureMigration::with_program("/usr/local/bin/confiture");
/// let _ = (adapter, pinned);
/// ```
pub struct ConfitureMigration {
    program: OsString,
}

impl Default for ConfitureMigration {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfitureMigration {
    /// Create an adapter that spawns the `confiture` binary.
    ///
    /// The binary is taken from the `FRAISIER_CONFITURE_BIN` environment variable
    /// when set, otherwise `confiture` is resolved on `PATH`.
    #[must_use]
    pub fn new() -> Self {
        let program = std::env::var_os(PROGRAM_ENV)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("confiture"));
        Self { program }
    }

    /// Create an adapter that spawns the binary at `program`.
    #[must_use]
    pub fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// Spawn a `migrate` subcommand, returning the captured outcome.
    async fn run(
        &self,
        subcommand: &str,
        ctx: &AdapterCtx,
        extra: &[OsString],
    ) -> Result<RunOutput, AdapterError> {
        let output_path = temp_output_path(subcommand);
        let plan = plan(subcommand, ctx, extra, &output_path)?;

        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&plan.args)
            .env(CONFITURE_DSN_ENV, &plan.database_url)
            .current_dir(&ctx.workdir);

        let result = command.output().await;
        let output = match result {
            Ok(output) => output,
            Err(spawn_err) => {
                let _ = tokio::fs::remove_file(&output_path).await;
                return Err(self.spawn_error(subcommand, &spawn_err));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let json = read_report_json(&output_path, &stdout).await;
        let _ = tokio::fs::remove_file(&output_path).await;

        Ok(RunOutput {
            code: output.status.code(),
            stdout,
            stderr,
            json,
        })
    }

    /// Error for a failure to even spawn the `confiture` binary.
    fn spawn_error(&self, operation: &str, cause: &std::io::Error) -> AdapterError {
        let program = self.program.to_string_lossy();
        AdapterError {
            adapter: Some(ADAPTER_NAME.to_owned()),
            operation: Some(operation.to_owned()),
            ..AdapterError::new(
                AdapterErrorKind::Execution,
                format!("failed to spawn '{program}': {cause}"),
            )
        }
    }
}

/// Build the argument vector and resolve the DSN for a `migrate` subcommand.
///
/// The DSN is returned separately (to be injected as `CONFITURE_DATABASE_URL`)
/// and is *never* placed in `args` — that separation is the load-bearing
/// secret-handling guarantee.
fn plan(
    subcommand: &str,
    ctx: &AdapterCtx,
    extra: &[OsString],
    output: &Path,
) -> Result<PlannedInvocation, AdapterError> {
    let database_url = ctx
        .secret(DATABASE_URL_LOGICAL)
        .map_err(|err| err.with_adapter(ADAPTER_NAME))?;

    let mut args: Vec<OsString> = vec![OsString::from("migrate"), OsString::from(subcommand)];
    args.extend_from_slice(extra);
    // `--no-config`: the environment is the sole DSN source (Confiture #152),
    // so no `db/environments/*.yaml` can shadow the injected DSN.
    args.push(OsString::from("--no-config"));
    args.push(OsString::from("--format"));
    args.push(OsString::from("json"));
    // `--output`: Confiture writes clean JSON here while human progress goes to
    // stdout, so parsing never has to disentangle the two.
    args.push(OsString::from("--output"));
    args.push(output.as_os_str().to_owned());
    if let Some(migrations_path) = &ctx.migrations_path {
        if subcommand_takes_migrations_dir(subcommand) {
            args.push(OsString::from("--migrations-dir"));
            args.push(migrations_path.as_os_str().to_owned());
        }
    }

    Ok(PlannedInvocation { args, database_url })
}

/// Whether a `migrate` subcommand accepts `--migrations-dir`.
///
/// `current` reads only the tracking table from the database and rejects the
/// flag (it has no migration-file inputs); every other subcommand the adapter
/// drives takes it.
fn subcommand_takes_migrations_dir(subcommand: &str) -> bool {
    subcommand != "current"
}

/// A resolved `migrate` invocation: the argv (secret-free) and the DSN to inject
/// as an environment variable.
struct PlannedInvocation {
    args: Vec<OsString>,
    database_url: String,
}

// Manual, redacting `Debug` so the resolved DSN can never reach a log or panic
// message — only the (secret-free) argv is shown.
impl std::fmt::Debug for PlannedInvocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlannedInvocation")
            .field("args", &self.args)
            .field("database_url", &"<redacted>")
            .finish()
    }
}

/// The captured result of one `confiture migrate` invocation.
struct RunOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    json: Option<Value>,
}

impl RunOutput {
    /// Whether the process exited 0.
    fn succeeded(&self) -> bool {
        self.code == Some(0)
    }

    /// Build an [`AdapterError`] for a non-success exit of `operation`.
    fn into_error(self, operation: &str) -> AdapterError {
        let kind = kind_for_code(self.code);
        let detail = self
            .json
            .as_ref()
            .and_then(report_detail)
            .unwrap_or_else(|| first_nonempty_line(&self.stderr));
        let code_repr = self
            .code
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        let mut message = format!("`confiture migrate {operation}` exited with {code_repr}");
        if !detail.is_empty() {
            message.push_str(": ");
            message.push_str(&detail);
        }
        if self.code == Some(LOCK_EXIT_CODE) {
            message.push_str(" (migration lock held by another process — retriable)");
        }
        AdapterError {
            adapter: Some(ADAPTER_NAME.to_owned()),
            operation: Some(operation.to_owned()),
            stderr: (!self.stderr.trim().is_empty()).then(|| self.stderr.clone()),
            ..AdapterError::new(kind, message)
        }
    }
}

/// Confiture's retriable lock-contention exit code (`LOCK_1300`).
const LOCK_EXIT_CODE: i32 = 6;

/// Map a Confiture process exit code to an [`AdapterErrorKind`].
///
/// Codes 2 (validation/config) and 5 (`CONFIG_010`, no usable DSN) are
/// configuration problems; everything else (1 generic, 3 execution, 6 lock,
/// 8 missing `.down.sql`, signals) is an execution failure.
const fn kind_for_code(code: Option<i32>) -> AdapterErrorKind {
    match code {
        Some(2 | 5) => AdapterErrorKind::InvalidConfig,
        _ => AdapterErrorKind::Execution,
    }
}

/// Extract a human-readable failure detail from a Confiture JSON report,
/// handling both report shapes: `{ "errors": ["…"] }` (apply/rollback) and
/// `{ "error": { "message": "…" } }` (the structured error boundary).
fn report_detail(json: &Value) -> Option<String> {
    if let Some(errors) = json.get("errors").and_then(Value::as_array) {
        let messages: Vec<String> = errors
            .iter()
            .filter_map(|value| value.as_str().map(ToOwned::to_owned))
            .collect();
        if !messages.is_empty() {
            return Some(messages.join("; "));
        }
    }
    if let Some(error) = json.get("error") {
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            return Some(message.to_owned());
        }
        if let Some(message) = error.as_str() {
            return Some(message.to_owned());
        }
    }
    None
}

/// Whether a Confiture JSON report is the structured "tracking table absent"
/// (uninitialised database) error, which the adapter treats as "no revision".
fn reports_uninitialised(json: Option<&Value>) -> bool {
    json.and_then(|report| report.get("error"))
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        == Some(UNINITIALISED_ERROR_CODE)
}

/// The first non-blank line of `text`, trimmed (used for error messages).
fn first_nonempty_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// A unique temp path for a subcommand's `--output` JSON report.
fn temp_output_path(subcommand: &str) -> PathBuf {
    let seq = OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let safe: String = subcommand
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("fraisier-confiture-{pid}-{seq}-{safe}.json"))
}

/// Read the JSON report, preferring the clean `--output` file and falling back to
/// the (possibly progress-prefixed) stdout for commands that emit pure JSON.
async fn read_report_json(output_path: &Path, stdout: &str) -> Option<Value> {
    if let Ok(bytes) = tokio::fs::read(output_path).await {
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            return Some(value);
        }
    }
    serde_json::from_str::<Value>(stdout.trim()).ok()
}

/// Parse `migrate current` JSON into the current revision (`None` when the
/// tracking table is empty).
fn parse_current_revision(json: &Value) -> Option<Revision> {
    json.get("revision")
        .and_then(Value::as_str)
        .map(Revision::new)
}

/// Build a [`MigrationOutcome`] from a `migrate up` JSON report.
fn parse_up_outcome(json: &Value, log: String) -> MigrationOutcome {
    let applied = versions_from(json.get("applied"), "version");
    let to = applied.last().cloned();
    MigrationOutcome {
        from: None,
        to,
        applied,
        log,
    }
}

/// Build a [`MigrationOutcome`] from a `migrate down-to` JSON report.
fn parse_down_to_outcome(json: &Value, log: String) -> MigrationOutcome {
    let from = json.get("from").and_then(Value::as_str).map(Revision::new);
    let to = json.get("to").and_then(Value::as_str).map(Revision::new);
    let applied = json
        .get("rolled_back")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|value| value.as_str().map(Revision::new))
                .collect()
        })
        .unwrap_or_default();
    MigrationOutcome {
        from,
        to,
        applied,
        log,
    }
}

/// Extract `Revision`s from an array of `{ <field>: "<rev>" }` objects.
fn versions_from(array: Option<&Value>, field: &str) -> Vec<Revision> {
    array
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get(field).and_then(Value::as_str).map(Revision::new))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a `migrate verify` JSON report into a [`VerifyReport`].
fn parse_verify_report(json: &Value) -> VerifyReport {
    let failed = json
        .get("failed_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let checks = json
        .get("results")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(verify_check_from).collect())
        .unwrap_or_default();
    VerifyReport {
        ok: failed == 0,
        checks,
    }
}

/// Convert one Confiture verify `results[]` entry to a [`VerifyCheck`].
fn verify_check_from(result: &Value) -> VerifyCheck {
    let version = result.get("version").and_then(Value::as_str).unwrap_or("");
    let name = result.get("name").and_then(Value::as_str).unwrap_or("");
    let status = result.get("status").and_then(Value::as_str).unwrap_or("");
    let label = if name.is_empty() || name == version {
        version.to_owned()
    } else {
        format!("{version}_{name}")
    };
    let detail = result
        .get("error")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| (status != "verified").then(|| status.to_owned()));
    VerifyCheck {
        name: label,
        ok: status != "failed",
        detail,
    }
}

/// Parse a `migrate preflight` JSON report into a [`PreflightReport`].
fn parse_preflight_report(json: &Value) -> PreflightReport {
    let ok = json.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let issues = json
        .get("issues")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(preflight_issue_from).collect())
        .unwrap_or_default();
    PreflightReport { ok, issues }
}

/// Convert one Confiture preflight `issues[]` entry to a [`PreflightIssue`].
fn preflight_issue_from(issue: &Value) -> PreflightIssue {
    let severity = match issue.get("severity").and_then(Value::as_str) {
        Some("error") => Severity::Error,
        Some("warning") => Severity::Warning,
        _ => Severity::Info,
    };
    PreflightIssue {
        severity,
        code: issue
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        message: issue
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        migration: issue
            .get("migration")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    }
}

/// Parse the version out of `confiture --version` output (`confiture version X`).
fn parse_version(output: &str) -> String {
    output
        .split_whitespace()
        .last()
        .unwrap_or("unknown")
        .to_owned()
}

#[async_trait]
impl MigrationAdapter for ConfitureMigration {
    async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
        let output = tokio::process::Command::new(&self.program)
            .arg("--version")
            .output()
            .await
            .map_err(|err| self.spawn_error("describe", &err))?;
        if !output.status.success() {
            return Err(RunOutput {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                json: None,
            }
            .into_error("describe"));
        }
        let version = parse_version(&String::from_utf8_lossy(&output.stdout));
        Ok(AdapterDescription {
            name: ADAPTER_NAME.to_owned(),
            version,
            protocol_version: PROTOCOL_VERSION,
            capabilities: CAPABILITIES.iter().map(|cap| (*cap).to_owned()).collect(),
        })
    }

    async fn current_revision(&self, ctx: &AdapterCtx) -> Result<Option<Revision>, AdapterError> {
        let output = self.run("current", ctx, &[]).await?;
        if output.succeeded() {
            return Ok(output.json.as_ref().and_then(parse_current_revision));
        }
        // A reachable-but-uninitialised database has no current revision — but
        // only when Confiture actually reports that (PRECON_1001), not for an
        // unrelated exit 2 such as a usage error.
        if reports_uninitialised(output.json.as_ref()) {
            return Ok(None);
        }
        Err(output.into_error("current"))
    }

    async fn up(
        &self,
        ctx: &AdapterCtx,
        target: Option<Revision>,
    ) -> Result<MigrationOutcome, AdapterError> {
        let extra = target.map_or_else(Vec::new, |revision| {
            vec![OsString::from("--target"), OsString::from(revision.0)]
        });
        let output = self.run("up", ctx, &extra).await?;
        if output.succeeded() {
            let outcome = output
                .json
                .as_ref()
                .map_or_else(MigrationOutcome::default, |json| {
                    parse_up_outcome(json, output.stdout.clone())
                });
            return Ok(outcome);
        }
        Err(output.into_error("up"))
    }

    async fn down_to(
        &self,
        ctx: &AdapterCtx,
        target: Revision,
    ) -> Result<MigrationOutcome, AdapterError> {
        let extra = [OsString::from(target.0)];
        let output = self.run("down-to", ctx, &extra).await?;
        if output.succeeded() {
            let outcome = output
                .json
                .as_ref()
                .map_or_else(MigrationOutcome::default, |json| {
                    parse_down_to_outcome(json, output.stdout.clone())
                });
            return Ok(outcome);
        }
        Err(output.into_error("down-to"))
    }

    async fn verify(&self, ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
        let output = self.run("verify", ctx, &[]).await?;
        // A report (even one with failures) is a valid result, not an error;
        // only the inability to produce one is an adapter error.
        if let Some(json) = &output.json {
            return Ok(parse_verify_report(json));
        }
        if output.succeeded() {
            return Ok(VerifyReport {
                ok: true,
                checks: Vec::new(),
            });
        }
        Err(output.into_error("verify"))
    }

    async fn preflight(&self, ctx: &AdapterCtx) -> Result<PreflightReport, AdapterError> {
        let output = self.run("preflight", ctx, &[]).await?;
        if let Some(json) = &output.json {
            return Ok(parse_preflight_report(json));
        }
        Err(output.into_error("preflight"))
    }
}

/// Whether `args` contains `needle` as a whole argument.
#[cfg(test)]
fn args_contain(args: &[OsString], needle: &str) -> bool {
    args.iter().any(|arg| arg == std::ffi::OsStr::new(needle))
}

#[cfg(test)]
mod tests {
    use super::{
        args_contain, kind_for_code, parse_current_revision, parse_down_to_outcome,
        parse_preflight_report, parse_up_outcome, parse_verify_report, parse_version, plan,
        reports_uninitialised, subcommand_takes_migrations_dir, ConfitureMigration,
        CONFITURE_DSN_ENV,
    };
    use fraisier_core::adapter_axes::{AdapterCtx, AdapterErrorKind, Revision, Severity};
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    /// Serialises env-mutating tests so `set_var`/`var` don't race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const SECRET_DSN: &str = "postgresql://user:s3cr3t@db.internal:5432/app";

    fn ctx_with_secret(source_var: &str) -> AdapterCtx {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.workdir = PathBuf::from("/srv/app");
        ctx.migrations_path = Some(PathBuf::from("/srv/app/db/migrations"));
        ctx.env_secrets
            .insert("DATABASE_URL".to_owned(), source_var.to_owned());
        ctx
    }

    #[test]
    fn plan_injects_dsn_in_env_never_argv() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let source = "FRAISIER_CONF_TEST_DSN_A";
        std::env::set_var(source, SECRET_DSN);
        let ctx = ctx_with_secret(source);
        let out = PathBuf::from("/tmp/report.json");

        let plan = plan("current", &ctx, &[], &out).expect("plan resolves the secret");

        std::env::remove_var(source);

        // The DSN must be the injected env value...
        assert_eq!(plan.database_url, SECRET_DSN);
        // ...and must NEVER appear in argv (Decision 5).
        assert!(
            !plan.args.iter().any(|arg| arg == OsStr::new(SECRET_DSN)),
            "secret DSN leaked into argv: {:?}",
            plan.args
        );
        // The env-only DSN mode and JSON output must be requested.
        assert!(args_contain(&plan.args, "--no-config"));
        assert!(args_contain(&plan.args, "--format"));
        assert!(args_contain(&plan.args, "json"));
        assert!(args_contain(&plan.args, "--output"));
        // The subcommand is shaped as `migrate current`.
        assert_eq!(plan.args[0], OsStr::new("migrate"));
        assert_eq!(plan.args[1], OsStr::new("current"));
    }

    #[test]
    fn plan_passes_migrations_dir_when_present() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let source = "FRAISIER_CONF_TEST_DSN_B";
        std::env::set_var(source, SECRET_DSN);
        let ctx = ctx_with_secret(source);

        let plan = plan("up", &ctx, &[], &PathBuf::from("/tmp/r.json")).expect("plan");
        std::env::remove_var(source);

        assert!(args_contain(&plan.args, "--migrations-dir"));
        assert!(plan
            .args
            .iter()
            .any(|arg| arg == OsStr::new("/srv/app/db/migrations")));
    }

    #[test]
    fn plan_omits_migrations_dir_when_absent() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let source = "FRAISIER_CONF_TEST_DSN_C";
        std::env::set_var(source, SECRET_DSN);
        let mut ctx = ctx_with_secret(source);
        ctx.migrations_path = None;

        let plan = plan("up", &ctx, &[], &PathBuf::from("/tmp/r.json")).expect("plan");
        std::env::remove_var(source);

        assert!(!args_contain(&plan.args, "--migrations-dir"));
    }

    #[test]
    fn plan_current_never_passes_migrations_dir() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let source = "FRAISIER_CONF_TEST_DSN_D";
        std::env::set_var(source, SECRET_DSN);
        // ctx_with_secret sets migrations_path, yet `current` rejects the flag.
        let ctx = ctx_with_secret(source);
        assert!(ctx.migrations_path.is_some());

        let plan = plan("current", &ctx, &[], &PathBuf::from("/tmp/r.json")).expect("plan");
        std::env::remove_var(source);

        assert!(!subcommand_takes_migrations_dir("current"));
        assert!(subcommand_takes_migrations_dir("up"));
        assert!(
            !args_contain(&plan.args, "--migrations-dir"),
            "current must not receive --migrations-dir: {:?}",
            plan.args
        );
    }

    #[test]
    fn uninitialised_report_is_detected() {
        let precon = serde_json::json!({
            "ok": false,
            "error": { "code": "PRECON_1001", "message": "tracking table absent" }
        });
        assert!(reports_uninitialised(Some(&precon)));

        // A different error (or a usage error with no JSON) is NOT uninitialised.
        let other = serde_json::json!({ "ok": false, "error": { "code": "CONFIG_010" } });
        assert!(!reports_uninitialised(Some(&other)));
        assert!(!reports_uninitialised(None));
    }

    #[test]
    fn plan_fails_loudly_when_secret_undeclared() {
        // No env_secrets mapping for DATABASE_URL.
        let ctx = AdapterCtx::new("checkout", "production");
        let err = plan("current", &ctx, &[], &PathBuf::from("/tmp/r.json"))
            .expect_err("missing secret must fail before spawning");
        assert_eq!(err.kind, AdapterErrorKind::MissingSecret);
        assert_eq!(err.adapter.as_deref(), Some("confiture"));
    }

    #[test]
    fn exit_codes_map_to_kinds() {
        assert_eq!(kind_for_code(Some(2)), AdapterErrorKind::InvalidConfig);
        assert_eq!(kind_for_code(Some(5)), AdapterErrorKind::InvalidConfig);
        assert_eq!(kind_for_code(Some(3)), AdapterErrorKind::Execution);
        assert_eq!(kind_for_code(Some(6)), AdapterErrorKind::Execution);
        assert_eq!(kind_for_code(Some(8)), AdapterErrorKind::Execution);
        assert_eq!(kind_for_code(Some(1)), AdapterErrorKind::Execution);
        assert_eq!(kind_for_code(None), AdapterErrorKind::Execution);
    }

    #[test]
    fn current_revision_parses_value_and_null() {
        let applied = serde_json::json!({
            "revision": "002", "name": "more",
            "applied_at": "2026-06-02T05:36:19+02:00", "checksum": "abc"
        });
        assert_eq!(parse_current_revision(&applied), Some(Revision::new("002")));

        let empty = serde_json::json!({
            "revision": null, "name": null, "applied_at": null, "checksum": null
        });
        assert_eq!(parse_current_revision(&empty), None);
    }

    #[test]
    fn up_outcome_collects_applied_versions() {
        let json = serde_json::json!({
            "success": true,
            "applied": [
                { "version": "001", "name": "init", "duration_ms": 3 },
                { "version": "002", "name": "more", "duration_ms": 1 }
            ],
            "errors": []
        });
        let outcome = parse_up_outcome(&json, "log".to_owned());
        assert_eq!(
            outcome.applied,
            vec![Revision::new("001"), Revision::new("002")]
        );
        assert_eq!(outcome.to, Some(Revision::new("002")));
        assert_eq!(outcome.log, "log");
    }

    #[test]
    fn down_to_outcome_maps_rolled_back() {
        let json = serde_json::json!({
            "from": "002", "to": "001", "rolled_back": ["002"], "skipped": [], "errors": []
        });
        let outcome = parse_down_to_outcome(&json, String::new());
        assert_eq!(outcome.from, Some(Revision::new("002")));
        assert_eq!(outcome.to, Some(Revision::new("001")));
        assert_eq!(outcome.applied, vec![Revision::new("002")]);
    }

    #[test]
    fn verify_report_reflects_failed_count() {
        let json = serde_json::json!({
            "verified_count": 1, "failed_count": 0, "skipped_count": 1, "total_applied": 2,
            "results": [
                { "version": "001", "name": "init", "status": "verified", "error": null },
                { "version": "002", "name": "002", "status": "no_file", "error": null }
            ]
        });
        let report = parse_verify_report(&json);
        assert!(report.ok);
        assert_eq!(report.checks.len(), 2);
        assert_eq!(report.checks[0].name, "001_init");
        assert!(report.checks[0].ok);

        let failing = serde_json::json!({
            "failed_count": 1,
            "results": [{ "version": "003", "name": "x", "status": "failed", "error": "boom" }]
        });
        let report = parse_verify_report(&failing);
        assert!(!report.ok);
        assert!(!report.checks[0].ok);
        assert_eq!(report.checks[0].detail.as_deref(), Some("boom"));
    }

    #[test]
    fn preflight_report_maps_issues_and_severity() {
        let json = serde_json::json!({
            "ok": true,
            "summary": { "errors": 0, "warnings": 1, "info": 0, "migrations_checked": 3 },
            "issues": [{
                "severity": "warning",
                "code": "PFLIGHT_NON_TRANSACTIONAL",
                "message": "Migration 003 has non-transactional statement(s)",
                "migration": "003"
            }]
        });
        let report = parse_preflight_report(&json);
        assert!(report.ok);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].severity, Severity::Warning);
        assert_eq!(report.issues[0].code, "PFLIGHT_NON_TRANSACTIONAL");
        assert_eq!(report.issues[0].migration.as_deref(), Some("003"));
    }

    #[test]
    fn version_is_the_last_token() {
        assert_eq!(parse_version("confiture version 0.20.0"), "0.20.0");
        assert_eq!(parse_version("confiture version 0.20.0\n"), "0.20.0");
    }

    #[test]
    fn program_override_is_honoured() {
        let adapter = ConfitureMigration::with_program("/opt/confiture");
        assert_eq!(adapter.program, OsString::from("/opt/confiture"));
        // The env-injection env var name is the canonical Confiture var.
        assert_eq!(CONFITURE_DSN_ENV, "CONFITURE_DATABASE_URL");
    }
}

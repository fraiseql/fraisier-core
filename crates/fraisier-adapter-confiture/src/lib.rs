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
//! ## Version requirements
//!
//! Confiture ≥ 0.20.0 is the floor for `current` / `up` / `down-to` / `verify`:
//! it provides `migrate current`, `migrate down-to`, and the `--no-config`
//! env-only DSN mode this adapter depends on. The **`preflight`** capability
//! additionally requires **Confiture ≥ 0.22.0** — earlier versions reject the
//! `--output` flag this adapter passes to every subcommand, so `migrate preflight`
//! could not emit its JSON report. Since `preflight` is an advertised capability
//! and the deploy layer enables the forward-compat lint by default,
//! **≥ 0.22.0 is the effective minimum for a default deploy.** Confiture
//! 0.22 also froze its exit-code / JSON shapes as a stability contract aligned to
//! this adapter (`docs/reference/fraisier-adapter-contract.md` in the Confiture
//! repo, mirrored by its `tests/contract/test_fraisier_adapter_surface.py`).
//!
//! The **`window_safe`** capability (the first-class blue-green forward-compat
//! verdict, parsed from the `preflight` report's top-level `window_safe` boolean)
//! requires **Confiture ≥ 0.23.0** (fraiseql/confiture#154). The verdict is
//! purely forward-compatibility for a two-version window — `false` for any
//! replica-unsafe op or any migration the classifier cannot read (`.py`), `true`
//! for online-safe ops including `CREATE INDEX CONCURRENTLY`. An older confiture
//! omits the field; fraisier's blue-green gate then refuses (fail-safe).
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

mod exit_codes;
use exit_codes::{classify, NO_LEDGER_ERROR_CODE};

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
const CAPABILITIES: &[&str] = &[
    "current_revision",
    "up",
    "down_to",
    "verify",
    "preflight",
    "window_safe",
];

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
        let class = classify(self.code, self.json.as_ref().and_then(error_code_of));
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
        // Lock contention is the one retriable class; the nuance rides in the
        // message (the wire kind stays a generic Execution — see
        // `ExitClass::to_adapter_kind`).
        if class.is_retriable() {
            message.push_str(" (migration lock held by another process — retriable)");
        }
        AdapterError {
            adapter: Some(ADAPTER_NAME.to_owned()),
            operation: Some(operation.to_owned()),
            stderr: (!self.stderr.trim().is_empty()).then(|| self.stderr.clone()),
            ..AdapterError::new(class.to_adapter_kind(), message)
        }
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

/// Whether a Confiture JSON payload is the structured *error envelope* its
/// failure boundary emits, rather than the report the subcommand produces when
/// it works.
///
/// This matters because Confiture writes the envelope to the **same**
/// `--output` file a report would go to, on every error path — so "we got JSON
/// back" says nothing about whether the command succeeded. An envelope carries
/// a top-level `error` object and none of a report's counts; read as a verify
/// report it would yield "0 failures", i.e. a pass.
///
/// The test is the presence of that object alone, not the absence of some
/// report field: a payload wrongly judged an envelope merely becomes a loud
/// adapter error, while a payload wrongly judged a report passes the ship gate.
/// A successful report's `"error": null` is not an object, so it does not match.
fn is_error_envelope(json: &Value) -> bool {
    json.get("error").is_some_and(Value::is_object)
}

/// The symbolic error code from a Confiture error envelope (`error.code`), when
/// the payload carries one. Both the classifier ([`kind_for_code`]) and the
/// no-ledger check ([`reports_uninitialised`]) read the code through here.
fn error_code_of(json: &Value) -> Option<&str> {
    json.get("error")?.get("code")?.as_str()
}

/// Whether a Confiture JSON report is the structured "tracking table absent"
/// (uninitialised database) error, which the adapter treats as "no revision".
fn reports_uninitialised(json: Option<&Value>) -> bool {
    json.and_then(error_code_of) == Some(NO_LEDGER_ERROR_CODE)
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
    // Confiture's first-class window-safety verdict, when this version emits it
    // (top-level `window_safe` boolean). Absent on older confiture → `None`, so the
    // consumer falls back to the `PFLIGHT_REPLICA_*` issue codes.
    let window_safe = json.get("window_safe").and_then(Value::as_bool);
    PreflightReport {
        ok,
        issues,
        window_safe,
    }
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
        // A report (even one with failing checks) is a valid result, not an
        // error; only the inability to produce one is an adapter error. An
        // error envelope is *not* a report — see [`is_error_envelope`] — so it
        // falls through to the error path carrying Confiture's own diagnosis,
        // rather than being parsed into a green verdict.
        let report = output
            .json
            .as_ref()
            .filter(|json| !is_error_envelope(json))
            .map(parse_verify_report);
        if let Some(report) = report {
            return Ok(report);
        }
        if output.json.is_none() && output.succeeded() {
            return Ok(VerifyReport {
                ok: true,
                checks: Vec::new(),
            });
        }
        Err(output.into_error("verify"))
    }

    async fn preflight(&self, ctx: &AdapterCtx) -> Result<PreflightReport, AdapterError> {
        let output = self.run("preflight", ctx, &[]).await?;
        // Same envelope rule as `verify`. An envelope would at least parse to
        // `ok = false` here (it carries a top-level `"ok"`), so this is not the
        // silent-pass bug — but it would report a *clean* refusal: no issues and
        // no sign of the connection failure or missing ledger behind it.
        let report = output
            .json
            .as_ref()
            .filter(|json| !is_error_envelope(json))
            .map(parse_preflight_report);
        if let Some(report) = report {
            return Ok(report);
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
        args_contain, classify, error_code_of, is_error_envelope, parse_current_revision,
        parse_down_to_outcome, parse_preflight_report, parse_up_outcome, parse_verify_report,
        parse_version, plan, reports_uninitialised, subcommand_takes_migrations_dir,
        ConfitureMigration, CONFITURE_DSN_ENV,
    };
    use fraisier_core::adapter_axes::{AdapterCtx, AdapterErrorKind, Revision, Severity};
    use serde_json::Value;
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    /// The composed `envelope -> AdapterErrorKind` projection `into_error` applies:
    /// read `error.code` from the JSON, classify, project. Exercises `error_code_of`
    /// + `classify` + `to_adapter_kind` together, the way the adapter really does.
    fn kind_of(code: Option<i32>, json: Option<&Value>) -> AdapterErrorKind {
        classify(code, json.and_then(error_code_of)).to_adapter_kind()
    }

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
        // A faithful 1:1 projection of the canonical table (see `exit_codes`):
        // every confiture exit integer has its own wire kind.
        for (code, kind) in [
            (1, AdapterErrorKind::InternalError),
            (2, AdapterErrorKind::PreconditionFailed),
            (3, AdapterErrorKind::DbUnreachable),
            (4, AdapterErrorKind::SchemaError),
            (5, AdapterErrorKind::InvalidConfig),
            (6, AdapterErrorKind::LockContention),
            (7, AdapterErrorKind::GitError),
            (8, AdapterErrorKind::IrreversibleRollback),
        ] {
            assert_eq!(kind_of(Some(code), None), kind, "exit {code}");
        }
        // No exit code (killed by signal) is unclassifiable → internal.
        assert_eq!(kind_of(None, None), AdapterErrorKind::InternalError);
    }

    #[test]
    fn uninitialised_exit_two_is_a_precondition_not_a_config_error() {
        // Confiture exits 2 with PRECON_1001 for a database that has no migration
        // ledger. That is "never migrated" — its own PreconditionFailed kind, not
        // "your config is broken" (which would surface as JSON-RPC -32602 and send
        // the operator to edit a healthy config file).
        let no_ledger = serde_json::json!({
            "ok": false,
            "error": { "code": "PRECON_1001", "message": "No migration ledger found" }
        });
        assert_eq!(
            kind_of(Some(2), Some(&no_ledger)),
            AdapterErrorKind::PreconditionFailed
        );
        // Exit 2 always means "no ledger" under confiture's frozen contract, even
        // when the envelope is absent.
        assert_eq!(kind_of(Some(2), None), AdapterErrorKind::PreconditionFailed);

        // A genuine configuration problem (no usable DSN, CONFIG_010) exits 5 and
        // stays InvalidConfig — and a present exit code is never laundered by a
        // stray error code, so a severe exit 5 is not downgraded to a precondition.
        let bad_config = serde_json::json!({
            "ok": false,
            "error": { "code": "CONFIG_010", "message": "no usable database URL" }
        });
        assert_eq!(
            kind_of(Some(5), Some(&bad_config)),
            AdapterErrorKind::InvalidConfig
        );
        assert_eq!(
            kind_of(Some(5), Some(&no_ledger)),
            AdapterErrorKind::InvalidConfig
        );
    }

    #[test]
    fn error_envelopes_are_distinguished_from_reports() {
        // Every envelope Confiture's failure boundary writes, whatever the path.
        for code in ["CONFIG_006", "PRECON_1001", "INTERNAL_ERROR", "CONFIG_010"] {
            let envelope = serde_json::json!({
                "ok": false,
                "error": { "code": code, "message": "boom", "details": {} }
            });
            assert!(
                is_error_envelope(&envelope),
                "{code} envelope must never be read as a report"
            );
            // The trap this guards: an envelope has no counts, so parsing it as
            // a verify report yields zero failures — a pass.
            assert!(
                parse_verify_report(&envelope).ok,
                "{code}: the envelope still parses green, which is exactly why \
                 verify() must reject it before parsing"
            );
        }

        // Genuine reports are not envelopes — including a clean one whose
        // per-check `error` fields are null.
        let clean = serde_json::json!({
            "verified_count": 1, "failed_count": 0,
            "results": [{ "version": "001", "name": "init", "status": "verified", "error": null }]
        });
        assert!(!is_error_envelope(&clean));
        let failing = serde_json::json!({
            "failed_count": 1,
            "results": [{ "version": "003", "name": "x", "status": "failed", "error": "boom" }]
        });
        assert!(!is_error_envelope(&failing));
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
        // No `window_safe` field on this (0.22-shaped) report → None, so the gate
        // falls back to the PFLIGHT_REPLICA_* presence rule.
        assert_eq!(report.window_safe, None);
    }

    #[test]
    fn preflight_report_reads_the_first_class_window_safe_verdict() {
        // A confiture that emits the typed verdict (the cross-repo Phase-3 contract).
        let safe = parse_preflight_report(
            &serde_json::json!({ "ok": true, "window_safe": true, "issues": [] }),
        );
        assert_eq!(safe.window_safe, Some(true));
        let unsafe_ = parse_preflight_report(
            &serde_json::json!({ "ok": true, "window_safe": false, "issues": [] }),
        );
        assert_eq!(unsafe_.window_safe, Some(false));
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

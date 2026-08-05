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
//! The **`risk_tier`** capability (a per-change risk-tiered change-set, parsed
//! from the `preflight` report's `change_set` object) requires a confiture that
//! implements the migration risk contract — provisionally **≥ 0.40.0**, the
//! `RISK_TIER_MIN_CONFITURE` floor.
//! Unlike the capabilities above it is **advertised conditionally**, on the
//! version the installed binary reports: claiming it against a confiture that
//! cannot classify would make fraisier's policy gate expect a change-set and
//! deny every deploy. Withholding it says *"I do not classify"*, and a deploy
//! with no risk policy configured then behaves exactly as it does today. The
//! contract is specified in `docs/proposals/migration-risk-contract.md`.
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
    AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, ChangeSet, MigrationAdapter,
    MigrationOutcome, PreflightIssue, PreflightReport, Revision, SchemaChange, Severity,
    VerifyCheck, VerifyReport,
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

/// The capability advertised when the installed confiture classifies the
/// pending schema changes into a risk-tiered change-set.
///
/// One string, not two: a change-set without tiers gives the policy gate
/// nothing to decide on, and tiers without a change-set have nothing to attach
/// to, so there is no useful intermediate state to advertise.
const RISK_TIER_CAPABILITY: &str = "risk_tier";

/// The first confiture release that emits a change-set (fraiseql/confiture#197).
///
/// Gating on the **installed** version is what keeps the capability honest.
/// Advertising `risk_tier` against a confiture that cannot classify would make
/// the policy gate expect a change-set and deny every deploy — safe, but
/// useless. Withholding it is the honest *"I do not classify"*, which callers
/// handle deliberately, and which keeps a deploy with no risk policy working
/// exactly as it does today.
///
/// The floor is **provisional**: confiture 0.39.0 emits no change-set, so the
/// producer half of the contract lands no earlier than 0.40.0. It is confirmed
/// against the real binary when the two repositories land together.
const RISK_TIER_MIN_CONFITURE: (u32, u32, u32) = (0, 40, 0);

/// The `kind` given to a change entry this build could not read.
///
/// It is fraisier's own marker, never a confiture code: the adapter is saying
/// *"something is here and I could not classify it"*, which is a denial, not a
/// classification.
const UNPARSEABLE_KIND: &str = "unparseable";

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
    // `PreflightReport` is `#[non_exhaustive]`, so a struct literal — including
    // `..Default::default()` — is not available outside `fraisier-core`; the
    // builder is the supported construction path, and it is what makes the next
    // field on that struct additive here instead of breaking.
    let mut report = PreflightReport::new(ok).with_issues(issues);
    if let Some(window_safe) = window_safe {
        report = report.with_window_safe(window_safe);
    }
    if let Some(change_set) = parse_change_set(json) {
        report = report.with_change_set(change_set);
    }
    report
}

/// Parse the classified change-set out of a `migrate preflight` JSON report.
///
/// `None` means *nobody classified this*, which is never *safe* — it is the
/// state a pre-contract confiture, a producer bug, and an unreadable payload
/// all land in, and the policy gate denies on all three.
///
/// The two failure granularities are deliberately different, and the asymmetry
/// is load-bearing (contract §6):
///
/// - a broken **envelope** voids the whole change-set. If the wrapper is
///   untrustworthy, the entries inside it cannot be trusted either.
/// - a broken **entry** becomes an unclassified placeholder, never a hole.
///   Dropping it would shrink the set silently, and a shorter list of
///   fully-classified changes reads as a *cleaner* plan than the truth — the
///   one failure direction this contract exists to prevent.
///
/// Both resolve to *denied*. Only one of them could be mistaken for a clean
/// bill of health, and it is the one that is refused outright.
fn parse_change_set(json: &Value) -> Option<ChangeSet> {
    // Confiture writes its error envelope to the same `--output` file a report
    // goes to, on every failure path — so a crash could otherwise present as a
    // classification. See [`is_error_envelope`].
    if is_error_envelope(json) {
        return None;
    }
    // No key at all: a confiture older than this contract. Expected, and not an
    // event worth warning about — the capability handshake already says so.
    let raw = json.get("change_set")?;
    let Some(envelope) = raw.as_object() else {
        warn_unusable("change_set", "an object", raw);
        return None;
    };
    let Some(raw_version) = envelope.get("contract_version") else {
        tracing::warn!(
            "confiture preflight: the change-set carries no `contract_version`, so it cannot be \
             read; the pending schema changes count as unclassified"
        );
        return None;
    };
    // A `0` is an *unstamped* payload rather than one from an older contract
    // (majors start at 1), and it would sail past the `usable_change_set`
    // version check as though a producer had stamped it.
    let Some(contract_version) = raw_version
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .filter(|version| *version != 0)
    else {
        warn_unusable("contract_version", "a contract revision", raw_version);
        return None;
    };
    let changes = match envelope.get("changes") {
        // Absent is equivalent to empty: the producer classified and found
        // nothing (contract §4). Anything that is not a list, `null` included,
        // leaves us unable to enumerate the changes at all — which is an
        // envelope-level break, not an empty plan.
        None => Vec::new(),
        Some(Value::Array(entries)) => entries
            .iter()
            .enumerate()
            .map(|(index, entry)| schema_change_from(index, entry))
            .collect(),
        Some(other) => {
            warn_unusable("changes", "a list of changes", other);
            return None;
        }
    };
    Some(ChangeSet::new(changes).with_contract_version(contract_version))
}

/// Convert one Confiture preflight `changes[]` entry to a [`SchemaChange`].
///
/// The entry is handed to [`SchemaChange`]'s own deserializer rather than read
/// field by field, so the rule that an unrecognised tier is *unclassified* and
/// never a nearest match lives in exactly one place — `fraisier-core`, where
/// the taxonomy is defined and where every other adapter's report is parsed.
/// A second copy of that rule here is a second place for it to drift.
///
/// An entry that will not deserialize at all becomes an
/// [`unclassified_placeholder`] rather than an error: one unreadable change
/// does not invalidate the ones beside it.
fn schema_change_from(index: usize, change: &Value) -> SchemaChange {
    serde_json::from_value::<SchemaChange>(change.clone())
        .unwrap_or_else(|_| unclassified_placeholder(index, change))
}

/// A stand-in for a change entry this build could not read, holding its place
/// in the plan.
///
/// Dropping the entry would shrink the set silently, and a shorter list of
/// fully-classified changes reads as a *cleaner* plan than the truth — the one
/// failure direction the contract exists to prevent (§6). The placeholder
/// carries no tier, so it is unclassified, so the policy gate denies on it and
/// can name it. A hole is denied by nothing.
///
/// It names the entry's **position** and the **shape** that arrived, and
/// quotes nothing from inside it: a payload that is off the contract has also
/// left the contract's promise that `detail` carries no DSN and no credential.
fn unclassified_placeholder(index: usize, change: &Value) -> SchemaChange {
    let shape = json_shape(change);
    tracing::warn!(
        "confiture preflight: the change entry at index {index} is {shape}, not a schema change; \
         recording it as an unclassified change rather than dropping it from the plan"
    );
    SchemaChange::new(
        UNPARSEABLE_KIND,
        format!("<unreadable entry at index {index}>"),
    )
    .with_detail(format!(
        "the migration adapter emitted {shape} where a change entry was expected"
    ))
}

/// Warn that a change-set payload could not be read, naming the **shape** that
/// arrived and never its content.
///
/// A payload that is off the contract has also left the contract's promise that
/// `detail` carries no DSN and no credential, so nothing from inside it is
/// quotable into a log line.
fn warn_unusable(field: &str, expected: &str, value: &Value) {
    tracing::warn!(
        "confiture preflight: `{field}` is {}, not {expected}; the change-set is unusable and \
         the pending schema changes count as unclassified",
        json_shape(value)
    );
}

/// The JSON type name of `value` — its shape, never its content.
const fn json_shape(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
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

/// The capabilities to advertise for an installed confiture at `version`.
///
/// [`CAPABILITIES`] is the static base — every method this adapter implements
/// against every confiture it supports at all. `risk_tier` is the one that
/// depends on which binary is actually installed.
fn capabilities_for(version: &str) -> Vec<String> {
    let mut capabilities: Vec<String> = CAPABILITIES
        .iter()
        .map(|capability| (*capability).to_owned())
        .collect();
    if supports_risk_tier(version) {
        capabilities.push(RISK_TIER_CAPABILITY.to_owned());
    }
    capabilities
}

/// Whether the confiture at `version` can emit a risk-tiered change-set.
///
/// A version string this build cannot read degrades to `false` — *"I do not
/// classify"* — and never to `true`. Guessing upward would advertise a
/// capability the installed binary cannot fulfil, which is the one failure this
/// gate exists to prevent.
fn supports_risk_tier(version: &str) -> bool {
    version_triple(version).is_some_and(|triple| triple >= RISK_TIER_MIN_CONFITURE)
}

/// The `(major, minor, patch)` of a plain numeric version string, comparable as
/// a tuple — `"0.100.0"` outranks `"0.40.0"`, which as strings it does not.
///
/// Deliberately strict: one to three dot-separated decimal components and
/// nothing else. A pre-release or dev build (`0.40.0rc1`, `0.40.0.dev0`) is not
/// a release whose behaviour this adapter can vouch for, so it reads as no
/// version at all — which withholds the capability rather than granting it.
fn version_triple(version: &str) -> Option<(u32, u32, u32)> {
    let mut components = version.split('.');
    let major: u32 = components.next()?.parse().ok()?;
    let minor: u32 = components.next().map_or(Ok(0), str::parse).ok()?;
    let patch: u32 = components.next().map_or(Ok(0), str::parse).ok()?;
    components.next().is_none().then_some((major, minor, patch))
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
        // The capability list describes the binary that just answered, not this
        // crate: `risk_tier` is withheld unless that binary can classify.
        let capabilities = capabilities_for(&version);
        Ok(AdapterDescription {
            name: ADAPTER_NAME.to_owned(),
            version,
            protocol_version: PROTOCOL_VERSION,
            capabilities,
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
        args_contain, capabilities_for, classify, error_code_of, is_error_envelope,
        parse_change_set, parse_current_revision, parse_down_to_outcome, parse_preflight_report,
        parse_up_outcome, parse_verify_report, parse_version, plan, reports_uninitialised,
        subcommand_takes_migrations_dir, supports_risk_tier, version_triple, ConfitureMigration,
        CONFITURE_DSN_ENV,
    };
    use fraisier_core::adapter_axes::{
        AdapterCtx, AdapterErrorKind, ChangeSetUnavailable, PreflightReport, Revision, RiskTier,
        Severity, RISK_CONTRACT_VERSION,
    };
    use serde_json::Value;
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    /// The golden fixtures of the cross-repo pact (`tests/fixtures/preflight/`),
    /// embedded rather than read at runtime: deleting one breaks the *build*,
    /// which is what their `_README.md` promises. Confiture asserts it emits
    /// these bytes; the tests below assert this adapter parses them.
    const FIXTURES: &[(&str, &str)] = &[
        (
            "v0-no-change-set",
            include_str!("../tests/fixtures/preflight/v0-no-change-set.json"),
        ),
        (
            "v1-empty",
            include_str!("../tests/fixtures/preflight/v1-empty.json"),
        ),
        (
            "v1-additive",
            include_str!("../tests/fixtures/preflight/v1-additive.json"),
        ),
        (
            "v1-mixed",
            include_str!("../tests/fixtures/preflight/v1-mixed.json"),
        ),
        (
            "v1-unknown-tier",
            include_str!("../tests/fixtures/preflight/v1-unknown-tier.json"),
        ),
        (
            "v1-missing-tier",
            include_str!("../tests/fixtures/preflight/v1-missing-tier.json"),
        ),
        (
            "v2-future",
            include_str!("../tests/fixtures/preflight/v2-future.json"),
        ),
        (
            "malformed",
            include_str!("../tests/fixtures/preflight/malformed.json"),
        ),
    ];

    /// One golden fixture, by name (without the `.json`).
    fn fixture(name: &str) -> Value {
        let (_, bytes) = FIXTURES
            .iter()
            .find(|(fixture, _)| *fixture == name)
            .unwrap_or_else(|| panic!("no golden fixture named {name}"));
        serde_json::from_str(bytes)
            .unwrap_or_else(|err| panic!("golden fixture {name} is not valid JSON: {err}"))
    }

    #[test]
    fn every_fixture_parses_without_panicking() {
        for (name, _) in FIXTURES {
            let json = fixture(name);
            // Whichever way a fixture breaks the change-set, it never breaks the
            // report: `ok`, `issues` and `window_safe` predate this contract and
            // the deploy already blocks on them.
            let report = parse_preflight_report(&json);
            assert!(report.ok, "{name}: every fixture is a clean lint result");
            assert_eq!(
                report.window_safe,
                Some(true),
                "{name}: window_safe still crosses the seam untouched"
            );
            // And reaching the classification never panics, however it is missing.
            let _ = parse_change_set(&json);
            let _ = report.usable_change_set();
        }
    }

    /// A preflight report carrying `change_set` verbatim.
    ///
    /// The consumer-robustness cases below are deliberately *not* golden
    /// fixtures: asking confiture to emit a corrupted envelope as a contract
    /// obligation would be nonsense (see the pact's `_README.md`). They test
    /// this parser, not the pact.
    fn report_with_change_set(change_set: &Value) -> Value {
        serde_json::json!({
            "ok": true,
            "window_safe": true,
            "issues": [],
            "change_set": change_set,
        })
    }

    /// The distinction the whole design rests on, read off the wire: *nobody
    /// classified this* and *the adapter classified and found nothing* are
    /// different states, and only one of them is safe.
    #[test]
    fn an_absent_change_set_and_an_empty_one_are_different_states() {
        // A pre-contract confiture: no key at all. Unknown, and unknown is never
        // safety — the policy gate must be able to tell this apart from "clean".
        assert_eq!(parse_change_set(&fixture("v0-no-change-set")), None);

        // A confiture that implements the contract and found nothing to change.
        let classified = parse_change_set(&fixture("v1-empty")).expect("v1-empty is classified");
        assert!(classified.changes.is_empty());
        assert_eq!(classified.contract_version, 1);
    }

    #[test]
    fn a_string_change_set_yields_none() {
        // `"change_set": "additive"` — a producer bug the consumer must survive.
        assert_eq!(parse_change_set(&fixture("malformed")), None);
        // ...and the report around it still parses, so a typo in a purely
        // additive field never fails a deploy that never asked for risk tiers.
        let report = parse_preflight_report(&fixture("malformed"));
        assert!(report.ok);
        assert_eq!(report.window_safe, Some(true));
    }

    #[test]
    fn a_missing_contract_version_yields_none() {
        // An unversioned payload is unreadable, not empty: without the version
        // there is no way to know what the entries even mean.
        assert_eq!(
            parse_change_set(&report_with_change_set(
                &serde_json::json!({ "changes": [] })
            )),
            None
        );
        // Control: the same envelope, stamped.
        assert!(parse_change_set(&report_with_change_set(
            &serde_json::json!({ "contract_version": 1, "changes": [] })
        ))
        .is_some());
    }

    #[test]
    fn a_non_integer_contract_version_yields_none() {
        for version in [
            serde_json::json!("1"),
            serde_json::json!(1.5),
            serde_json::json!(-1),
            serde_json::json!(true),
            serde_json::json!(null),
            // Larger than the contract's `u32` can express.
            serde_json::json!(u64::from(u32::MAX) + 1),
        ] {
            assert_eq!(
                parse_change_set(&report_with_change_set(&serde_json::json!({
                    "contract_version": version,
                    "changes": [],
                }))),
                None,
                "contract_version {version} is not a contract revision"
            );
        }
    }

    /// Majors start at 1, so a `0` is an *unstamped* payload rather than one
    /// from an older contract — and `0 <= RISK_CONTRACT_VERSION` would sail
    /// straight through `usable_change_set`'s version check. `fraisier-core`'s
    /// own lenient deserializer rejects it for the same reason; the two paths
    /// must not disagree about what a valid envelope is.
    #[test]
    fn a_zero_contract_version_yields_none() {
        assert_eq!(
            parse_change_set(&report_with_change_set(&serde_json::json!({
                "contract_version": 0,
                "changes": [],
            }))),
            None
        );
    }

    /// Confiture writes its error envelope to the same `--output` file a report
    /// would go to, on every failure path. A crash must not be able to present
    /// as a clean, empty plan — the same class of bug the `verify`/`preflight`
    /// envelope guards already close.
    #[test]
    fn an_error_envelope_never_yields_a_change_set() {
        let mut envelope = report_with_change_set(&serde_json::json!({
            "contract_version": 1,
            "changes": [],
        }));
        // Control: without the error object this payload classifies.
        assert!(parse_change_set(&envelope).is_some());

        envelope["ok"] = serde_json::json!(false);
        envelope["error"] = serde_json::json!({
            "code": "CONFIG_006",
            "message": "Failed to connect to database: connection refused",
        });
        assert!(is_error_envelope(&envelope));
        assert_eq!(
            parse_change_set(&envelope),
            None,
            "an error envelope carries no classification, however well-formed its payload looks"
        );
    }

    /// `changes` of the wrong type is an envelope-level break: we cannot
    /// enumerate the changes at all, so we cannot claim to have classified
    /// them. Reading it as `[]` would turn garbage into a clean, empty plan.
    #[test]
    fn a_changes_key_that_is_not_an_array_voids_the_envelope() {
        for changes in [
            serde_json::json!({}),
            serde_json::json!("add_column"),
            serde_json::json!(null),
            serde_json::json!(3),
        ] {
            assert_eq!(
                parse_change_set(&report_with_change_set(&serde_json::json!({
                    "contract_version": 1,
                    "changes": changes,
                }))),
                None,
                "changes: {changes} is not an enumerable change list"
            );
        }
        // Absent, though, *is* equivalent to empty: the producer classified and
        // found nothing (contract §4).
        let absent = parse_change_set(&report_with_change_set(
            &serde_json::json!({ "contract_version": 1 }),
        ))
        .expect("an absent `changes` is an empty one");
        assert!(absent.changes.is_empty());
    }

    /// A version from the future is *not* swallowed here. The adapter's job is
    /// to hand the consumer what the producer actually said; refusing it is
    /// `usable_change_set`'s job, and it can only name the version in the
    /// refusal if the version survives the parse. Restamping it with this
    /// build's version — which is what `ChangeSet::new` alone would do — would
    /// turn a payload we cannot read into one we silently approve.
    #[test]
    fn a_future_contract_version_is_preserved_for_the_consumer() {
        let set = parse_change_set(&fixture("v2-future")).expect("the envelope itself is readable");
        assert_eq!(set.contract_version, 2);

        let report = parse_preflight_report(&fixture("v2-future"));
        let refusal = report
            .usable_change_set()
            .expect_err("a change-set from a later contract is unusable");
        assert_eq!(
            refusal,
            ChangeSetUnavailable::VersionTooNew {
                found: 2,
                understood: RISK_CONTRACT_VERSION,
            }
        );
        assert!(
            refusal.to_string().contains('2'),
            "the refusal must name the version that arrived: {refusal}"
        );
    }

    #[test]
    fn v1_additive_parses_one_tiered_change() {
        let set = parse_change_set(&fixture("v1-additive")).expect("classified");
        assert_eq!(set.contract_version, RISK_CONTRACT_VERSION);
        assert_eq!(set.changes.len(), 1);

        let change = &set.changes[0];
        assert_eq!(change.kind, "add_column");
        assert_eq!(change.object, "public.tb_user.nickname");
        // The version prefix, not the filename — it is what `issues[].migration`
        // already carries, and what keeps the plan render's column bounded.
        assert_eq!(change.migration.as_deref(), Some("20260804120000"));
        assert_eq!(change.tier, Some(RiskTier::Additive));
        assert_eq!(
            change.detail.as_deref(),
            Some("ADD COLUMN nickname text NULL")
        );
        assert_eq!(set.worst_tier(), Some(RiskTier::Additive));
        assert_eq!(set.unclassified().count(), 0);
    }

    #[test]
    fn v1_mixed_preserves_order_and_tiers() {
        let set = parse_change_set(&fixture("v1-mixed")).expect("classified");

        // Migration order, exactly as the producer listed it. The contract does
        // not sort; the render does, and it cannot sort what it never received.
        let listed: Vec<(&str, Option<RiskTier>)> = set
            .changes
            .iter()
            .map(|change| (change.object.as_str(), change.tier))
            .collect();
        assert_eq!(
            listed,
            [
                ("public.tb_user.nickname", Some(RiskTier::Additive)),
                ("public.tb_order.idx_placed_at", Some(RiskTier::LockRisky)),
                ("public.tb_user.legacy_flag", Some(RiskTier::Irreversible)),
            ]
        );
        // The worst tier is computed, never taken from the last entry or the
        // producer's ordering.
        assert_eq!(set.worst_tier(), Some(RiskTier::Irreversible));
    }

    /// Two parsers read this same wire shape: this hand-rolled one, and
    /// `fraisier-core`'s `serde` path (which any IPC adapter's report arrives
    /// through). They must not drift — the same bytes have to classify the same
    /// way whichever adapter carried them.
    ///
    /// They diverge in exactly one deliberate place, which no golden fixture
    /// exercises: a *malformed entry* voids the whole set for `serde`'s
    /// all-or-nothing `Vec<SchemaChange>`, while this parser reads entries one
    /// at a time and can keep the rest beside an unclassified placeholder. Both
    /// deny; this one can say more about why.
    #[test]
    fn the_manual_parser_agrees_with_the_typed_deserializer() {
        for (name, bytes) in FIXTURES {
            let typed: PreflightReport =
                serde_json::from_str(bytes).unwrap_or_else(|err| panic!("{name}: {err}"));
            let manual = parse_preflight_report(&fixture(name));
            assert_eq!(
                manual.change_set, typed.change_set,
                "{name}: the adapter's parse and the typed contract disagree"
            );
        }
    }

    /// A future confiture tier — `"quantum"` — must not round down to the
    /// nearest string-similar one. One unclassified change is a denial the
    /// operator can act on; a misclassification is a wrong verdict nobody sees.
    #[test]
    fn an_unknown_tier_survives_as_unclassified() {
        let set = parse_change_set(&fixture("v1-unknown-tier")).expect("classified");
        assert_eq!(set.changes.len(), 2, "the entry beside it must survive");
        assert_eq!(set.changes[0].tier, Some(RiskTier::Additive));

        let unknown = &set.changes[1];
        assert_eq!(unknown.tier, None);
        // The entry itself is intact — an unreadable *tier* is not an
        // unreadable *change*, and the refusal has to be able to name it.
        assert_eq!(unknown.kind, "entangle_column");
        assert_eq!(unknown.object, "public.tb_user.spin_state");
        assert_eq!(
            set.unclassified()
                .map(|c| c.object.as_str())
                .collect::<Vec<_>>(),
            ["public.tb_user.spin_state"]
        );
        // Not folded into the worst tier: an unclassified change is not "tier
        // zero", and a set whose worst *known* tier is `additive` would
        // otherwise read as approvable.
        assert_eq!(set.worst_tier(), Some(RiskTier::Additive));
    }

    #[test]
    fn a_missing_tier_survives_as_unclassified() {
        let set = parse_change_set(&fixture("v1-missing-tier")).expect("classified");
        assert_eq!(set.changes.len(), 2);

        let untiered = &set.changes[1];
        assert_eq!(untiered.tier, None);
        assert_eq!(untiered.kind, "alter_column_type");
        assert_eq!(
            untiered.detail.as_deref(),
            Some("ALTER COLUMN total_cents TYPE bigint")
        );
        assert_eq!(set.unclassified().count(), 1);
    }

    /// The load-bearing asymmetry, from the dangerous side.
    ///
    /// A broken envelope voids everything; a broken *entry* must not simply
    /// vanish. A four-change plan silently rendered as three fully-classified
    /// changes reads *cleaner* than the truth, and that is the one failure
    /// direction this contract exists to prevent.
    #[test]
    fn a_malformed_entry_leaves_an_unclassified_placeholder_not_a_hole() {
        for (label, broken) in [
            ("an entry that is not an object", serde_json::json!(42)),
            (
                "an entry with no `kind`",
                serde_json::json!({ "object": "public.tb_user.email" }),
            ),
            (
                "an entry whose `object` is not a string",
                serde_json::json!({ "kind": "drop_column", "object": 42 }),
            ),
        ] {
            let set = parse_change_set(&report_with_change_set(&serde_json::json!({
                "contract_version": 1,
                "changes": [
                    { "kind": "add_column", "object": "public.tb_user.nickname", "tier": "additive" },
                    broken,
                    { "kind": "drop_column", "object": "public.tb_user.legacy_flag", "tier": "irreversible" },
                ],
            })))
            .expect("one bad entry does not void the envelope");

            assert_eq!(set.changes.len(), 3, "{label}: the plan must not shrink");
            // Position is preserved, so the surviving entries still line up
            // with what the producer listed.
            assert_eq!(set.changes[0].tier, Some(RiskTier::Additive));
            assert_eq!(set.changes[2].tier, Some(RiskTier::Irreversible));

            let placeholder = &set.changes[1];
            assert_eq!(placeholder.tier, None, "{label}: never classified");
            assert_eq!(placeholder.kind, "unparseable", "{label}");
            assert!(
                placeholder.object.contains('1'),
                "{label}: the placeholder must name its position; got {}",
                placeholder.object
            );
            let detail = placeholder
                .detail
                .as_deref()
                .unwrap_or_else(|| panic!("{label}: the placeholder must say what arrived"));
            // The shape, and only the shape: an entry that is off the contract
            // has also left the contract's promise that `detail` carries no
            // credential, so nothing from inside it is quotable.
            assert!(
                !detail.contains("42") && !detail.contains("tb_user"),
                "{label}: the placeholder quoted the payload: {detail}"
            );

            // And the gate can both see it and name it.
            assert_eq!(set.unclassified().count(), 1, "{label}");
            assert_eq!(set.worst_tier(), Some(RiskTier::Irreversible), "{label}");
        }
    }

    /// The pact is the *directory*, not this file's table: a state confiture
    /// starts emitting must not be silently unexercised here.
    #[test]
    fn the_fixture_table_covers_the_whole_pact_directory() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/preflight");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("the pact directory exists")
            .filter_map(|entry| {
                let path = entry.expect("readable dir entry").path();
                (path.extension()? == "json")
                    .then(|| path.file_stem()?.to_str().map(ToOwned::to_owned))?
            })
            .collect();
        on_disk.sort();
        let mut tabled: Vec<String> = FIXTURES
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect();
        tabled.sort();
        assert_eq!(
            on_disk, tabled,
            "every fixture in the pact directory must be exercised by name"
        );
    }

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

    /// The capability must describe the **installed** confiture, not this
    /// crate's ambitions. Advertising `risk_tier` against a binary that cannot
    /// classify makes the policy gate expect a change-set and deny every
    /// deploy — safe, and useless.
    #[test]
    fn describe_omits_risk_tier_on_an_old_confiture() {
        for version in ["0.22.0", "0.23.0", "0.38.1", "0.39.0"] {
            assert!(
                !supports_risk_tier(version),
                "confiture {version} emits no change-set"
            );
            let capabilities = capabilities_for(version);
            assert!(
                !capabilities.iter().any(|cap| cap == "risk_tier"),
                "confiture {version}: {capabilities:?}"
            );
            // The base handshake is untouched — an old confiture keeps every
            // capability it had.
            assert!(capabilities.iter().any(|cap| cap == "preflight"));
            assert!(capabilities.iter().any(|cap| cap == "window_safe"));
        }
    }

    #[test]
    fn describe_advertises_risk_tier_on_a_new_confiture() {
        for version in ["0.40.0", "0.40.1", "0.41.0", "1.0.0"] {
            assert!(
                supports_risk_tier(version),
                "confiture {version} classifies"
            );
            assert!(capabilities_for(version)
                .iter()
                .any(|cap| cap == "risk_tier"));
        }
    }

    /// A confiture that changes its `--version` format must degrade to *"I do
    /// not classify"*, never to *"I classify"*. Absence is the honest answer to
    /// a question we could not read.
    #[test]
    fn an_unparseable_version_omits_risk_tier() {
        for version in [
            // What `parse_version` yields for empty or unexpected output.
            "unknown",
            "",
            // Python-shaped pre-release and dev builds: not a release this
            // adapter can vouch for.
            "0.40.0rc1",
            "0.40.0.dev0",
            "0.40.0+local",
            "v0.40.0",
            "0..0",
            "0.40.0-beta.1",
        ] {
            assert!(
                !supports_risk_tier(version),
                "an unreadable version ({version:?}) must not advertise a capability"
            );
        }
    }

    /// The floor is a numeric comparison, not a lexicographic one: as strings,
    /// `"0.100.0" < "0.40.0"`, which would silently withdraw the capability the
    /// first time confiture's minor reaches three digits.
    #[test]
    fn the_capability_floor_is_a_numeric_comparison() {
        assert!(supports_risk_tier("0.100.0"));
        assert!(supports_risk_tier("0.40"), "a missing patch reads as .0");
        assert!(!supports_risk_tier("0.9.0"));
        assert_eq!(version_triple("0.40.1"), Some((0, 40, 1)));
        assert_eq!(version_triple("1"), Some((1, 0, 0)));
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

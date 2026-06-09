//! # fraisier-adapter-command
//!
//! The **universal escape-hatch** migration adapter (PRD §6.3): a
//! [`MigrationAdapter`] that runs user-configured shell commands. It lets any
//! migration tool fraisier does not natively wrap be driven through the same
//! frozen trait — "if you can run it from a shell, you can deploy it".
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
//! ## Secrets and the target revision (never in argv)
//!
//! Every declared secret in [`AdapterCtx::env_secrets`] is resolved via
//! [`AdapterCtx::secret`] and exported to the command's environment under its
//! logical name (so a command can read `$DATABASE_URL`). The target revision for
//! `up`/`down_to` is exported as `FRAISIER_TARGET`. Neither secrets nor the
//! target are ever placed in argv — consistent with Decision 5.

use std::collections::BTreeMap;
use std::ffi::OsString;

use async_trait::async_trait;
use fraisier_adapter_support::{error, run_command, Captured};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, MigrationAdapter,
    MigrationOutcome, Revision, VerifyCheck, VerifyReport,
};
use serde_json::Value;

/// The adapter's default identity name.
const DEFAULT_NAME: &str = "command";

/// The IPC protocol major version this adapter's contract matches.
const PROTOCOL_VERSION: u32 = 1;

/// The env var the target revision is exported under for `up`/`down_to`.
const TARGET_ENV: &str = "FRAISIER_TARGET";

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

#[cfg(test)]
mod tests {
    use super::{resolve_secret_env, CommandMigration, CommandSpec};
    use fraisier_core::adapter_axes::{AdapterCtx, MigrationAdapter};
    use serde_json::{json, Value};
    use std::collections::BTreeMap;
    use std::ffi::OsString;

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

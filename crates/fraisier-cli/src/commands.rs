//! The command handlers. Each returns a [`CommandOutput`] (an exit code plus
//! pretty and JSON renderings) so the same logic serves both output modes and is
//! testable without spawning the binary.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;

use anyhow::{Context as _, Result};
use fraisier_config::{DeployConfig, Severity, ValidationReport};
use fraisier_core::single_host::{DeployRecord, SingleHostDeploy};
use fraisier_saga::saga::SagaOutcome;
use fraisier_saga::state_store::{FilesystemStateStore, FraiseKey, StateStore};
use serde_json::{json, Value};

use crate::factory;

/// The result of a command: an exit code and both renderings of its output.
pub(crate) struct CommandOutput {
    /// The process exit code.
    pub exit_code: i32,
    /// The human-readable rendering.
    pub pretty: String,
    /// The machine-readable rendering (printed under `--json`).
    pub json: Value,
}

fn load(config_path: &Path) -> Result<DeployConfig> {
    let toml = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    DeployConfig::from_toml_str(&toml)
        .with_context(|| format!("parsing config {}", config_path.display()))
}

fn render_issues(report: &ValidationReport) -> String {
    let mut out = report
        .issues
        .iter()
        .map(|issue| {
            let tag = match issue.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            format!("  [{tag}] {}: {}", issue.path, issue.message)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// `validate-config`: parse, expand, and validate, reporting every located issue.
pub(crate) fn validate_config(config_path: &Path) -> Result<CommandOutput> {
    let toml = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    match DeployConfig::from_toml_str(&toml) {
        Err(error) => Ok(CommandOutput {
            exit_code: 1,
            pretty: format!("config parse error:\n{error}\n"),
            json: json!({ "ok": false, "parse_error": error.to_string() }),
        }),
        Ok(config) => {
            let report = config.validate();
            let ok = report.ok();
            let mut pretty = render_issues(&report);
            pretty.push_str(match (ok, report.issues.is_empty()) {
                (true, true) => "config is valid\n",
                (true, false) => "config is valid (warnings only)\n",
                (false, _) => "config is INVALID\n",
            });
            Ok(CommandOutput {
                exit_code: i32::from(!ok),
                pretty,
                json: json!({ "ok": ok, "issues": serde_json::to_value(&report.issues)? }),
            })
        }
    }
}

/// Discover `fraisier-adapter-*` executables across the directories in `path_var`.
pub(crate) fn discover_adapters(path_var: Option<&OsStr>) -> Vec<String> {
    let mut names = BTreeSet::new();
    let Some(path_var) = path_var else {
        return Vec::new();
    };
    for dir in std::env::split_paths(path_var) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name
                .to_str()
                .and_then(|n| n.strip_prefix("fraisier-adapter-"))
            else {
                continue;
            };
            if !name.is_empty() && is_executable(&entry) {
                names.insert(name.to_owned());
            }
        }
    }
    names.into_iter().collect()
}

fn is_executable(entry: &std::fs::DirEntry) -> bool {
    let Ok(metadata) = entry.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `adapter list`: the migration adapters discoverable on `PATH`.
pub(crate) fn adapter_list() -> CommandOutput {
    let names = discover_adapters(std::env::var_os("PATH").as_deref());
    let pretty = if names.is_empty() {
        "no fraisier-adapter-* binaries found on PATH\n".to_owned()
    } else {
        let mut listing = names.join("\n");
        listing.push('\n');
        listing
    };
    CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({ "adapters": names }),
    }
}

/// `adapter describe <name>`: spawn the adapter and run the `describe` handshake.
pub(crate) async fn adapter_describe(name: &str) -> Result<CommandOutput> {
    use fraisier_core::adapter_axes::MigrationAdapter as _;

    let program = format!("fraisier-adapter-{name}");
    let adapter = fraisier_ipc::IpcMigrationAdapter::new(&program, name);
    match adapter.describe().await {
        Ok(description) => Ok(CommandOutput {
            exit_code: 0,
            pretty: format!(
                "{} v{} (protocol v{})\n  capabilities: {}\n",
                description.name,
                description.version,
                description.protocol_version,
                description.capabilities.join(", "),
            ),
            json: serde_json::to_value(&description)?,
        }),
        Err(error) => Ok(CommandOutput {
            exit_code: 1,
            pretty: format!("could not describe adapter '{name}' ({program}): {error}\n"),
            json: json!({ "adapter": name, "error": error.to_string() }),
        }),
    }
}

/// `status`: the recorded saga state and release ledger for the config's deploy.
pub(crate) async fn status(config_path: &Path, state_dir: &Path) -> Result<CommandOutput> {
    let config = load(config_path)?;
    let deploy = config.deploy.as_ref().context("missing [deploy] section")?;
    let fraise = deploy.name.clone().context("[deploy].name is required")?;
    let environment = deploy
        .environment
        .clone()
        .context("[deploy].environment is required")?;

    let store = FilesystemStateStore::new(state_dir)
        .with_context(|| format!("opening state store at {}", state_dir.display()))?;
    let key = FraiseKey::new(&fraise, &environment);
    let state = store.current_state(&key).await?;
    let ledger = store.current_snapshot(&key).await?;

    let mut pretty = state.as_ref().map_or_else(
        || format!("{fraise}/{environment}: no recorded state\n"),
        |state| {
            format!(
                "{fraise}/{environment}: {:?} (recorded {})\n",
                state.state, state.recorded_at
            )
        },
    );
    if let Some(ledger) = &ledger {
        if let Ok(record) = serde_json::from_value::<DeployRecord>(ledger.clone()) {
            let active = record
                .active
                .as_ref()
                .map_or_else(|| "<none>".to_owned(), |a| a.artifact.id.clone());
            let revision = record
                .revision
                .as_ref()
                .map_or_else(|| "<none>".to_owned(), ToString::to_string);
            pretty = format!("{pretty}  active artifact: {active}\n  revision: {revision}\n");
        }
    }

    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({
            "fraise": fraise,
            "environment": environment,
            "state": serde_json::to_value(&state)?,
            "ledger": ledger.unwrap_or(Value::Null),
        }),
    })
}

fn render_summary(summary: &factory::PlanSummary) -> String {
    let mut lines = vec![
        format!(
            "deploy plan for {}/{} → host {}",
            summary.fraise, summary.environment, summary.host
        ),
        format!("  artifact:  {}", summary.artifact),
        format!("  migration: {}", summary.migration),
        format!("  service:   {}", summary.service),
        format!("  health:    {}", summary.health),
    ];
    if let Some(source) = &summary.database_url_env {
        lines.push(format!("  database_url_env: {source}"));
    }
    lines.push(format!("  settings:  {}", summary.settings_keys.join(", ")));
    lines.push("(dry run — nothing was executed)".to_owned());
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// `deploy`: validate, then either summarize the plan (`--dry-run`) or run the
/// single-host deploy against the state store.
pub(crate) async fn deploy(
    config_path: &Path,
    state_dir: &Path,
    host: Option<&str>,
    app_version: Option<&str>,
    dry_run: bool,
) -> Result<CommandOutput> {
    let config = load(config_path)?;
    let report = config.validate();
    if !report.ok() {
        let mut pretty = render_issues(&report);
        pretty.push_str("refusing to deploy an invalid config\n");
        return Ok(CommandOutput {
            exit_code: 1,
            pretty,
            json: json!({ "ok": false, "issues": serde_json::to_value(&report.issues)? }),
        });
    }

    if dry_run {
        let summary = factory::summarize(&config, host, app_version)?;
        return Ok(CommandOutput {
            exit_code: 0,
            pretty: render_summary(&summary),
            json: serde_json::to_value(&summary)?,
        });
    }

    let resolved = factory::build(&config, host, app_version)?;
    let store = FilesystemStateStore::new(state_dir)
        .with_context(|| format!("opening state store at {}", state_dir.display()))?;
    let plan = SingleHostDeploy::builder(
        resolved.fraise.clone(),
        resolved.environment.clone(),
        resolved.host.clone(),
    )
    .context(resolved.ctx)
    .artifact(resolved.artifact)
    .migration(resolved.migration)
    .service(resolved.service)
    .health(resolved.health)
    .build()?;

    let outcome = plan.run(store).await?;
    let (exit_code, label, detail) = match &outcome {
        SagaOutcome::Committed => (0, "committed", String::new()),
        SagaOutcome::RolledBack {
            failed_step,
            reason,
        } => (
            1,
            "rolled_back",
            format!(" (step '{failed_step}': {reason})"),
        ),
        SagaOutcome::PartialRollback { reason } => (2, "partial_rollback", format!(" ({reason})")),
        _ => (1, "unknown", String::new()),
    };
    Ok(CommandOutput {
        exit_code,
        pretty: format!(
            "deploy of {}/{} {label}{detail}\n",
            resolved.fraise, resolved.environment
        ),
        json: json!({ "outcome": label, "detail": detail.trim() }),
    })
}

#[cfg(test)]
mod tests {
    use super::{adapter_list, deploy, discover_adapters, status, validate_config};
    use fraisier_core::single_host::DeployRecord;
    use fraisier_saga::events::SagaState;
    use fraisier_saga::state_store::{
        DeploymentState, FilesystemStateStore, FraiseKey, StateStore,
    };
    use std::path::Path;

    const VALID: &str = r#"
[deploy]
name = "checkout"
environment = "staging"

[artifact]
source = "release"
release_url = "https://example.com/checkout-{version}.tar.gz"
checksum_url = "https://example.com/checkout-{version}.tar.gz.sha256"

[migration]
adapter = "confiture"
database_url_env = "CHECKOUT_DATABASE_URL"

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
"#;

    fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write file");
        path
    }

    #[test]
    fn validate_config_accepts_a_valid_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "fraisier.toml", VALID);
        let out = validate_config(&path).expect("run");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["ok"], serde_json::json!(true));
    }

    #[test]
    fn validate_config_rejects_and_locates_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        // confiture without database_url_env.
        let bad = VALID.replace("database_url_env = \"CHECKOUT_DATABASE_URL\"\n", "");
        let path = write(dir.path(), "fraisier.toml", &bad);
        let out = validate_config(&path).expect("run");
        assert_eq!(out.exit_code, 1);
        let issues = out.json["issues"].as_array().expect("issues array");
        assert!(issues
            .iter()
            .any(|i| i["path"] == "migration.database_url_env"));
    }

    #[test]
    fn discover_adapters_finds_prefixed_executables() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("fraisier-adapter-demo");
        std::fs::write(&bin, "#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        // A non-adapter file is ignored.
        std::fs::write(dir.path().join("other-tool"), "x").expect("write");

        let found = discover_adapters(Some(dir.path().as_os_str()));
        assert_eq!(found, vec!["demo".to_owned()]);
    }

    #[test]
    fn adapter_list_runs_without_error() {
        // Whatever is on PATH, the command must produce valid output.
        let out = adapter_list();
        assert_eq!(out.exit_code, 0);
        assert!(out.json.get("adapters").is_some());
    }

    #[tokio::test]
    async fn deploy_dry_run_resolves_the_plan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", VALID);
        let state = dir.path().join("state");
        let out = deploy(&config, &state, Some("127.0.0.1"), Some("1.2.3"), true)
            .await
            .expect("dry run");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(
            out.json["migration"],
            serde_json::json!("confiture (in-process)")
        );
        assert_eq!(out.json["host"], serde_json::json!("127.0.0.1"));
    }

    #[tokio::test]
    async fn deploy_refuses_an_invalid_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = VALID.replace("database_url_env = \"CHECKOUT_DATABASE_URL\"\n", "");
        let config = write(dir.path(), "fraisier.toml", &bad);
        let state = dir.path().join("state");
        let out = deploy(&config, &state, None, None, false)
            .await
            .expect("run");
        assert_eq!(out.exit_code, 1);
        assert_eq!(out.json["ok"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn status_reports_recorded_state_and_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", VALID);
        let state_dir = dir.path().join("state");
        let store = FilesystemStateStore::new(&state_dir).expect("store");
        let key = FraiseKey::new("checkout", "staging");
        store
            .record_state(
                &key,
                &DeploymentState::new(SagaState::Committed, Some("rev-7".to_owned())),
            )
            .await
            .expect("record state");
        let ledger = DeployRecord {
            active: None,
            revision: Some(fraisier_core::adapter_axes::Revision::new("rev-7")),
        };
        store
            .record_snapshot(&key, &serde_json::to_value(&ledger).expect("encode"))
            .await
            .expect("record ledger");

        let out = status(&config, &state_dir).await.expect("status");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.json["fraise"], serde_json::json!("checkout"));
        assert!(out.pretty.contains("Committed"), "pretty: {}", out.pretty);
    }
}

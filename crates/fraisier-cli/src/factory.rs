//! Turning a validated [`DeployConfig`] into concrete adapter instances.
//!
//! This is the crate-graph wiring point: the migration adapter name decides
//! whether to use an **in-process** adapter (`confiture`, `command`) or to spawn
//! an **IPC** adapter (`fraisier-adapter-<name>`). The two paths handle the
//! `DATABASE_URL` secret differently, both honouring Decision 5:
//!
//! - **in-process**: the [`AdapterCtx`] carries `env_secrets["DATABASE_URL"] =
//!   <database_url_env>` (the *source* var name); the adapter resolves the value
//!   itself via `AdapterCtx::secret`.
//! - **IPC**: the CLI resolves the source var to its value *now* and injects it on
//!   the child as `DATABASE_URL=<value>`, sending `env_secrets` as the identity
//!   map `{DATABASE_URL -> DATABASE_URL}`. The source var name never crosses the
//!   process boundary.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context as _, Result};
use fraisier_config::DeployConfig;
use fraisier_core::adapter_axes::{
    AdapterCtx, ArtifactAdapter, HealthAdapter, HostId, MigrationAdapter, ServiceAdapter,
};
use serde_json::Value;

/// The logical secret name every migration adapter resolves the DSN under.
const DATABASE_URL_LOGICAL: &str = "DATABASE_URL";

/// A read-only summary of the adapters a deploy would use — what `--dry-run`
/// prints. Building it resolves no secrets and constructs no adapters.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlanSummary {
    /// The fraise (deployable) name.
    pub fraise: String,
    /// The target environment.
    pub environment: String,
    /// The host the single-host deploy targets.
    pub host: String,
    /// The selected artifact adapter, rendered (e.g. `"release"`).
    pub artifact: String,
    /// The selected migration adapter, rendered with its path (in-process / IPC).
    pub migration: String,
    /// The selected service adapter.
    pub service: String,
    /// The selected health adapter.
    pub health: String,
    /// The settings keys assembled for the shared [`AdapterCtx`].
    pub settings_keys: Vec<String>,
    /// The source env var the DSN is read from, if configured.
    pub database_url_env: Option<String>,
}

/// The fully-built adapters and context, ready to hand to a `SingleHostDeploy`.
pub struct ResolvedDeploy {
    /// The fraise (deployable) name.
    pub fraise: String,
    /// The target environment.
    pub environment: String,
    /// The host the deploy targets.
    pub host: HostId,
    /// The shared adapter context.
    pub ctx: AdapterCtx,
    /// The artifact adapter.
    pub artifact: Arc<dyn ArtifactAdapter>,
    /// The migration adapter (in-process or IPC).
    pub migration: Arc<dyn MigrationAdapter>,
    /// The service adapter.
    pub service: Arc<dyn ServiceAdapter>,
    /// The health adapter.
    pub health: Arc<dyn HealthAdapter>,
}

/// Resolve the single host the deploy targets.
///
/// # Errors
/// Fails if the config is multi-host and no explicit `host` override was given —
/// multi-host execution is Phase 4.
pub fn resolve_host(config: &DeployConfig, host_override: Option<&str>) -> Result<HostId> {
    if let Some(host) = host_override {
        return Ok(HostId::new(host));
    }
    if config.hosts.is_some() {
        bail!(
            "this config declares [hosts] (multi-host); multi-host deploy is not implemented \
             until Phase 4. Pass --host <address> to deploy a single host."
        );
    }
    Ok(HostId::new("localhost"))
}

/// Assemble the shared settings map every axis reads its own keys from. The keys
/// do not collide across axes (`unit` / `url` / `release_url` / …).
fn settings_map(config: &DeployConfig, app_version: Option<&str>) -> BTreeMap<String, Value> {
    let mut settings = BTreeMap::new();
    if let Some(artifact) = &config.artifact {
        put_str(
            &mut settings,
            "release_url",
            artifact.release_url.as_deref(),
        );
        put_str(
            &mut settings,
            "checksum_url",
            artifact.checksum_url.as_deref(),
        );
        put_str(&mut settings, "sha256", artifact.checksum.as_deref());
        put_path(
            &mut settings,
            "active_path",
            artifact.active_path.as_deref(),
        );
        put_path(
            &mut settings,
            "staging_dir",
            artifact.staging_dir.as_deref(),
        );
    }
    if let Some(version) = app_version {
        settings.insert("version".to_owned(), Value::String(version.to_owned()));
    }
    if let Some(service) = &config.service {
        put_str(&mut settings, "unit", service.unit.as_deref());
    }
    if let Some(health) = &config.health {
        put_str(&mut settings, "url", health.url.as_deref());
        settings.insert(
            "expected_status".to_owned(),
            Value::from(config.health_expected_status()),
        );
    }
    settings
}

fn put_str(map: &mut BTreeMap<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), Value::String(value.to_owned()));
    }
}

fn put_path(map: &mut BTreeMap<String, Value>, key: &str, value: Option<&std::path::Path>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), Value::String(value.display().to_string()));
    }
}

fn render_migration(config: &DeployConfig) -> String {
    match config.migration.as_ref().and_then(|m| m.adapter.as_deref()) {
        Some(name @ ("confiture" | "command")) => format!("{name} (in-process)"),
        Some(name) => format!("{name} (IPC: fraisier-adapter-{name})"),
        None => "<none>".to_owned(),
    }
}

fn axis_name(adapter: Option<&str>) -> String {
    adapter.unwrap_or("<none>").to_owned()
}

/// Build the read-only plan summary (no secrets resolved, no adapters built).
///
/// # Errors
/// Fails if the host cannot be resolved (multi-host config without `--host`).
pub fn summarize(
    config: &DeployConfig,
    host_override: Option<&str>,
    app_version: Option<&str>,
) -> Result<PlanSummary> {
    let deploy = config.deploy.as_ref();
    let host = resolve_host(config, host_override)?;
    let settings = settings_map(config, app_version);
    Ok(PlanSummary {
        fraise: deploy
            .and_then(|d| d.name.clone())
            .unwrap_or_else(|| "<none>".to_owned()),
        environment: deploy
            .and_then(|d| d.environment.clone())
            .unwrap_or_else(|| "<none>".to_owned()),
        host: host.as_str().to_owned(),
        artifact: axis_name(config.artifact.as_ref().and_then(|a| a.source.as_deref())),
        migration: render_migration(config),
        service: axis_name(config.service.as_ref().and_then(|s| s.adapter.as_deref())),
        health: axis_name(config.health.as_ref().and_then(|h| h.adapter.as_deref())),
        settings_keys: settings.into_keys().collect(),
        database_url_env: config
            .migration
            .as_ref()
            .and_then(|m| m.database_url_env.clone()),
    })
}

/// Build the concrete adapters and [`AdapterCtx`] for a single-host deploy.
///
/// # Errors
/// Fails if a required section/field is missing, an adapter name is unsupported
/// in this build, or (IPC path) the configured DSN env var is unset.
pub fn build(
    config: &DeployConfig,
    host_override: Option<&str>,
    app_version: Option<&str>,
) -> Result<ResolvedDeploy> {
    let deploy = config.deploy.as_ref().context("missing [deploy] section")?;
    let fraise = deploy.name.clone().context("[deploy].name is required")?;
    let environment = deploy
        .environment
        .clone()
        .context("[deploy].environment is required")?;
    let host = resolve_host(config, host_override)?;
    let settings = settings_map(config, app_version);

    let artifact = build_artifact(config)?;
    let service = build_service(config)?;
    let health = build_health(config)?;

    let database_url_env = config
        .migration
        .as_ref()
        .and_then(|m| m.database_url_env.clone());
    let mut env_secrets = BTreeMap::new();
    let migration = build_migration(config, database_url_env.as_deref(), &mut env_secrets)?;

    let mut ctx = AdapterCtx::new(fraise.clone(), environment.clone());
    ctx.host = Some(host.clone());
    ctx.workdir = PathBuf::from(".");
    ctx.migrations_path = config
        .migration
        .as_ref()
        .and_then(|m| m.migrations_path.clone());
    ctx.env_secrets = env_secrets;
    ctx.settings = settings;

    Ok(ResolvedDeploy {
        fraise,
        environment,
        host,
        ctx,
        artifact,
        migration,
        service,
        health,
    })
}

fn build_artifact(config: &DeployConfig) -> Result<Arc<dyn ArtifactAdapter>> {
    match config.artifact.as_ref().and_then(|a| a.source.as_deref()) {
        Some("release") => Ok(Arc::new(fraisier_artifact_release::ReleaseArtifact::new())),
        Some(other) => bail!(
            "artifact source '{other}' is not available in this build (only 'release' ships \
             in-process in Phase 1)"
        ),
        None => bail!("[artifact].source is required"),
    }
}

fn build_service(config: &DeployConfig) -> Result<Arc<dyn ServiceAdapter>> {
    match config.service.as_ref().and_then(|s| s.adapter.as_deref()) {
        Some("systemd") => Ok(Arc::new(fraisier_adapter_systemd::SystemdService::new())),
        Some(other) => bail!(
            "service adapter '{other}' is not available in this build (only 'systemd' ships in \
             Phase 1)"
        ),
        None => bail!("[service].adapter is required"),
    }
}

fn build_health(config: &DeployConfig) -> Result<Arc<dyn HealthAdapter>> {
    match config.health.as_ref().and_then(|h| h.adapter.as_deref()) {
        Some("http") => Ok(Arc::new(fraisier_adapter_http::HttpHealth::new())),
        Some(other) => bail!(
            "health adapter '{other}' is not available in this build (only 'http' ships in Phase 1)"
        ),
        None => bail!("[health].adapter is required"),
    }
}

fn build_migration(
    config: &DeployConfig,
    database_url_env: Option<&str>,
    env_secrets: &mut BTreeMap<String, String>,
) -> Result<Arc<dyn MigrationAdapter>> {
    let name = config
        .migration
        .as_ref()
        .and_then(|m| m.adapter.as_deref())
        .context("[migration].adapter is required")?;
    match name {
        "confiture" => {
            // In-process: map the logical secret to its source env var name.
            if let Some(source) = database_url_env {
                env_secrets.insert(DATABASE_URL_LOGICAL.to_owned(), source.to_owned());
            }
            Ok(Arc::new(
                fraisier_adapter_confiture::ConfitureMigration::new(),
            ))
        }
        "command" => {
            if let Some(source) = database_url_env {
                env_secrets.insert(DATABASE_URL_LOGICAL.to_owned(), source.to_owned());
            }
            let cmd_settings = config
                .migration
                .as_ref()
                .map(|m| m.settings.clone())
                .unwrap_or_default();
            Ok(Arc::new(
                fraisier_adapter_command::CommandMigration::from_settings("command", &cmd_settings),
            ))
        }
        other => {
            // IPC: resolve the DSN value now and inject it on the child under the
            // logical name; the source var name never crosses the boundary.
            let program = format!("fraisier-adapter-{other}");
            let mut adapter = fraisier_ipc::IpcMigrationAdapter::new(&program, other);
            if let Some(source) = database_url_env {
                let value = std::env::var(source).with_context(|| {
                    format!(
                        "the configured database_url_env '{source}' is not set in the environment"
                    )
                })?;
                adapter = adapter.with_env(DATABASE_URL_LOGICAL, value);
                env_secrets.insert(
                    DATABASE_URL_LOGICAL.to_owned(),
                    DATABASE_URL_LOGICAL.to_owned(),
                );
            }
            Ok(Arc::new(adapter))
        }
    }
}

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
use fraisier_adapter_support::{SshTransport, Transport};
use fraisier_config::DeployConfig;
use fraisier_core::adapter_axes::{
    AdapterCtx, ArtifactAdapter, HealthAdapter, HostId, LbAdapter, MigrationAdapter, ServiceAdapter,
};
use fraisier_core::multi_host::{MultiHostPlan, RolloutStrategy};
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
    /// Whether to run the migration adapter's forward-compatibility `preflight`
    /// lint (`[migration].forward_compatible_lint`, default `true`).
    pub forward_compatible_lint: bool,
    /// The artifact adapter.
    pub artifact: Arc<dyn ArtifactAdapter>,
    /// The migration adapter (in-process or IPC).
    pub migration: Arc<dyn MigrationAdapter>,
    /// The service adapter.
    pub service: Arc<dyn ServiceAdapter>,
    /// The health adapter.
    pub health: Arc<dyn HealthAdapter>,
}

/// Resolve the single host the **single-host** builders target.
///
/// # Errors
/// Fails if the config declares `[hosts]` (multi-host) and no explicit `host`
/// override was given — the caller should take the multi-host path
/// ([`build_multi_host`]) instead, or pass `--host` to target one host directly.
pub fn resolve_host(config: &DeployConfig, host_override: Option<&str>) -> Result<HostId> {
    if let Some(host) = host_override {
        return Ok(HostId::new(host));
    }
    if config.hosts.is_some() {
        bail!(
            "this config declares [hosts] (multi-host); deploy it with the multi-host rollout \
             (no --host), or pass --host <address> to target a single host of the inventory."
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
        put_str(&mut settings, "name", service.name.as_deref());
        put_str(
            &mut settings,
            "compose_service",
            service.compose_service.as_deref(),
        );
        put_path(
            &mut settings,
            "compose_file",
            service.compose_file.as_deref(),
        );
        if let Some(user) = service.user {
            settings.insert("user".to_owned(), Value::Bool(user));
        }
    }
    if let Some(health) = &config.health {
        put_str(&mut settings, "url", health.url.as_deref());
        settings.insert(
            "expected_status".to_owned(),
            Value::from(config.health_expected_status()),
        );
    }
    if let Some(lb) = &config.lb {
        put_path(&mut settings, "config_path", lb.config_path.as_deref());
        put_str(&mut settings, "upstream", lb.upstream.as_deref());
    }
    settings
}

/// Build the host-execution transport for a deploy: a remote `ssh` transport for
/// a multi-host config (from the optional `[ssh]` section), or [`Transport::Local`]
/// for a single-host one — so a single-host deploy is byte-identical to before.
fn build_transport(config: &DeployConfig) -> Transport {
    if config.hosts.is_none() {
        return Transport::Local;
    }
    let mut ssh = SshTransport::new();
    if let Some(cfg) = &config.ssh {
        if let Some(user) = &cfg.user {
            ssh = ssh.with_user(user.clone());
        }
        if let Some(port) = cfg.port {
            ssh = ssh.with_port(port);
        }
        if let Some(identity) = &cfg.identity_path {
            ssh = ssh.with_identity(identity.clone());
        }
        if !cfg.options.is_empty() {
            ssh = ssh.with_options(cfg.options.clone());
        }
    }
    Transport::ssh(ssh)
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
    let service = build_service(config, &build_transport(config))?;
    let health = build_health(config)?;

    let database_url_env = config
        .migration
        .as_ref()
        .and_then(|m| m.database_url_env.clone());
    let mut env_secrets = BTreeMap::new();
    let migration = build_migration(config, database_url_env.as_deref(), &mut env_secrets)?;

    // Default on: the forward-compat lint runs whenever the adapter advertises it,
    // unless the operator opts out in the config.
    let forward_compatible_lint = config
        .migration
        .as_ref()
        .and_then(|m| m.forward_compatible_lint)
        .unwrap_or(true);

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
        forward_compatible_lint,
        artifact,
        migration,
        service,
        health,
    })
}

/// The fully-built inputs for a [`fraisier_core::multi_host::MultiHostDeploy`].
pub struct ResolvedMultiHost {
    /// The fraise (deployable) name.
    pub fraise: String,
    /// The target environment.
    pub environment: String,
    /// The inventory + rollout strategy resolved from `[hosts]`.
    pub plan: MultiHostPlan,
    /// The shared adapter context (host set per-host by the composition).
    pub ctx: AdapterCtx,
    /// Whether to run the forward-compatibility preflight lint.
    pub forward_compatible_lint: bool,
    /// The artifact adapter.
    pub artifact: Arc<dyn ArtifactAdapter>,
    /// The migration adapter (run once on the orchestrator).
    pub migration: Arc<dyn MigrationAdapter>,
    /// The service adapter (runs on each host over the transport).
    pub service: Arc<dyn ServiceAdapter>,
    /// The health adapter.
    pub health: Arc<dyn HealthAdapter>,
    /// The load-balancer adapter.
    pub lb: Arc<dyn LbAdapter>,
}

/// Build the concrete adapters + plan for a **multi-host** deploy.
///
/// The service axis is bound to the SSH transport (from `[ssh]`) so it runs on
/// each remote host; the migration runs once on the orchestrator (so the DSN
/// secret stays local). The artifact and load-balancer axes are still local —
/// the config validation warns about this.
///
/// # Errors
/// Fails if `[hosts]`/`[deploy]` is missing or incomplete, an adapter name is
/// unsupported, or (IPC migration) the configured DSN env var is unset.
pub fn build_multi_host(
    config: &DeployConfig,
    app_version: Option<&str>,
) -> Result<ResolvedMultiHost> {
    let deploy = config.deploy.as_ref().context("missing [deploy] section")?;
    let fraise = deploy.name.clone().context("[deploy].name is required")?;
    let environment = deploy
        .environment
        .clone()
        .context("[deploy].environment is required")?;
    let inventory = config
        .host_inventory()
        .context("a multi-host deploy requires a [hosts] inventory")?;
    let strategy = config
        .rollout_strategy()
        .unwrap_or(RolloutStrategy::Rolling(1));
    let plan = MultiHostPlan::new(inventory, strategy);

    let transport = build_transport(config);
    let settings = settings_map(config, app_version);

    let artifact = build_artifact(config)?;
    let service = build_service(config, &transport)?;
    let health = build_health(config)?;
    let lb = build_lb(config)?;

    let database_url_env = config
        .migration
        .as_ref()
        .and_then(|m| m.database_url_env.clone());
    let mut env_secrets = BTreeMap::new();
    let migration = build_migration(config, database_url_env.as_deref(), &mut env_secrets)?;

    let forward_compatible_lint = config
        .migration
        .as_ref()
        .and_then(|m| m.forward_compatible_lint)
        .unwrap_or(true);

    // The host is set per-host by the multi-host composition; leave it None here.
    let mut ctx = AdapterCtx::new(fraise.clone(), environment.clone());
    ctx.workdir = PathBuf::from(".");
    ctx.migrations_path = config
        .migration
        .as_ref()
        .and_then(|m| m.migrations_path.clone());
    ctx.env_secrets = env_secrets;
    ctx.settings = settings;

    Ok(ResolvedMultiHost {
        fraise,
        environment,
        plan,
        ctx,
        forward_compatible_lint,
        artifact,
        migration,
        service,
        health,
        lb,
    })
}

/// A read-only summary of a multi-host deploy — what `--dry-run` prints. Building
/// it resolves no secrets and constructs no adapters.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MultiHostSummary {
    /// The fraise (deployable) name.
    pub fraise: String,
    /// The target environment.
    pub environment: String,
    /// Each host rendered as `name (address)`, in rollout order.
    pub hosts: Vec<String>,
    /// The rollout strategy, rendered (e.g. `rolling(1)`).
    pub strategy: String,
    /// How the per-host commands reach each host (`ssh [user@]` or `local`).
    pub transport: String,
    /// The selected artifact adapter.
    pub artifact: String,
    /// The selected migration adapter.
    pub migration: String,
    /// The selected service adapter.
    pub service: String,
    /// The selected health adapter.
    pub health: String,
    /// The selected load-balancer adapter.
    pub lb: String,
}

/// Build the read-only multi-host plan summary (no secrets, no adapters built).
///
/// # Errors
/// Fails if `[deploy]`/`[hosts]` is missing or incomplete.
pub fn summarize_multi_host(
    config: &DeployConfig,
    app_version: Option<&str>,
) -> Result<MultiHostSummary> {
    let deploy = config.deploy.as_ref();
    let inventory = config
        .host_inventory()
        .context("a multi-host deploy requires a [hosts] inventory")?;
    let hosts = inventory
        .hosts()
        .iter()
        .map(|h| format!("{} ({})", h.host, h.address))
        .collect();
    let strategy = match config
        .rollout_strategy()
        .unwrap_or(RolloutStrategy::Rolling(1))
    {
        RolloutStrategy::AllAtOnce => "all-at-once".to_owned(),
        RolloutStrategy::Rolling(n) => format!("rolling({n})"),
        _ => "rolling(1)".to_owned(),
    };
    let transport = match build_transport(config) {
        Transport::Local => "local".to_owned(),
        Transport::Ssh(_) => config
            .ssh
            .as_ref()
            .and_then(|s| s.user.clone())
            .map_or_else(
                || "ssh (<host>)".to_owned(),
                |user| format!("ssh ({user}@<host>)"),
            ),
    };
    let _ = app_version;
    Ok(MultiHostSummary {
        fraise: deploy
            .and_then(|d| d.name.clone())
            .unwrap_or_else(|| "<none>".to_owned()),
        environment: deploy
            .and_then(|d| d.environment.clone())
            .unwrap_or_else(|| "<none>".to_owned()),
        hosts,
        strategy,
        transport,
        artifact: axis_name(config.artifact.as_ref().and_then(|a| a.source.as_deref())),
        migration: render_migration(config),
        service: axis_name(config.service.as_ref().and_then(|s| s.adapter.as_deref())),
        health: axis_name(config.health.as_ref().and_then(|h| h.adapter.as_deref())),
        lb: axis_name(config.lb.as_ref().and_then(|l| l.adapter.as_deref())),
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

fn build_service(config: &DeployConfig, transport: &Transport) -> Result<Arc<dyn ServiceAdapter>> {
    match config.service.as_ref().and_then(|s| s.adapter.as_deref()) {
        Some("systemd") => Ok(Arc::new(
            fraisier_adapter_systemd::SystemdService::new().with_transport(transport.clone()),
        )),
        Some("rc") => Ok(Arc::new(
            fraisier_adapter_rc::RcService::new().with_transport(transport.clone()),
        )),
        Some("docker-compose") => Ok(Arc::new(
            fraisier_adapter_docker_compose::DockerComposeService::new()
                .with_transport(transport.clone()),
        )),
        Some(other) => bail!(
            "service adapter '{other}' is not available in this build \
             (built-in: 'systemd', 'rc', 'docker-compose')"
        ),
        None => bail!("[service].adapter is required"),
    }
}

fn build_lb(config: &DeployConfig) -> Result<Arc<dyn LbAdapter>> {
    match config.lb.as_ref().and_then(|l| l.adapter.as_deref()) {
        Some("nginx") => Ok(Arc::new(fraisier_adapter_nginx::NginxLb::new())),
        Some(other) => bail!(
            "load-balancer adapter '{other}' is not available in this build (only 'nginx' ships)"
        ),
        None => {
            bail!("a multi-host deploy requires an [lb] adapter (the rollout drains/reattaches)")
        }
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

#[cfg(test)]
mod tests {
    use super::summarize;
    use fraisier_config::DeployConfig;

    /// `[service].user` reaches the shared settings the systemd adapter reads
    /// `user` from (it drives `systemctl --user`).
    #[test]
    fn service_user_is_plumbed_into_the_adapter_settings() {
        let toml = r#"
[deploy]
name = "app"
environment = "prod"

[service]
adapter = "systemd"
unit = "app.service"
user = true
"#;
        let config = DeployConfig::from_toml_str(toml).expect("parses");
        let summary = summarize(&config, None, None).expect("summarize");
        assert!(
            summary.settings_keys.iter().any(|k| k == "user"),
            "user is in the assembled settings: {:?}",
            summary.settings_keys,
        );
    }
}

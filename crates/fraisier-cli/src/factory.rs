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
use fraisier_ipc::{Launcher, SshLauncher};
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
        put_path(&mut settings, "path", artifact.path.as_deref());
        put_str(&mut settings, "repo", artifact.repo.as_deref());
        put_str(&mut settings, "ref", artifact.reference.as_deref());
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
        // The command health adapter reads these from ctx.settings at probe time
        // (config-blind, like HttpHealth). timeout_ms additionally makes HTTP's
        // probe timeout operator-settable.
        put_str(&mut settings, "command", health.command.as_deref());
        if let Some(timeout_ms) = health.timeout_ms {
            settings.insert("timeout_ms".to_owned(), Value::from(timeout_ms));
        }
    }
    if let Some(lb) = &config.lb {
        put_path(&mut settings, "config_path", lb.config_path.as_deref());
        put_str(&mut settings, "upstream", lb.upstream.as_deref());
        put_path(&mut settings, "include_dir", lb.include_dir.as_deref());
    }
    if let Some(bg) = &config.blue_green {
        // The nginx TrafficDirector reads `targets` = { <target>: [<server>, …] }.
        let blue = bg.blue.as_deref().unwrap_or("blue");
        let green = bg.green.as_deref().unwrap_or("green");
        let mut targets = serde_json::Map::new();
        if !bg.blue_servers.is_empty() {
            targets.insert(blue.to_owned(), Value::from(bg.blue_servers.clone()));
        }
        if !bg.green_servers.is_empty() {
            targets.insert(green.to_owned(), Value::from(bg.green_servers.clone()));
        }
        if !targets.is_empty() {
            settings.insert("targets".to_owned(), Value::Object(targets));
        }
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

/// Build the IPC adapter **launcher** (the `source = "release-ipc"` analog of
/// [`build_transport`]): an `ssh` launcher reaching each host for a multi-host
/// config (with `ControlMaster` connection reuse across the deploy's per-host
/// calls), or [`Launcher::Local`] for a single-host one. Reads the same `[ssh]`
/// parameters as the shell transport so both remote mechanisms share one config.
fn build_launcher(config: &DeployConfig) -> Launcher {
    if config.hosts.is_none() {
        return Launcher::Local;
    }
    let mut ssh = SshLauncher::new();
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
    // ControlMaster amortises connection setup across a deploy's stage/activate/
    // current calls per host. The socket dir must exist; if it can't be created,
    // fall back to one connection per call rather than failing the deploy.
    let control_dir = std::env::temp_dir().join("fraisier-ssh-cm");
    if std::fs::create_dir_all(&control_dir).is_ok() {
        ssh = ssh.with_control_dir(control_dir);
    }
    Launcher::ssh(ssh)
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

    // Local for a single-host config; an `[ssh]`-configured remote transport only
    // when `[hosts]` is present (host-pull artifact + service then run per host).
    let transport = build_transport(config);
    let launcher = build_launcher(config);
    let artifact = build_artifact(config, &transport, &launcher)?;
    let service = build_service(config, &transport)?;
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
    let launcher = build_launcher(config);
    let settings = settings_map(config, app_version);

    let artifact = build_artifact(config, &transport, &launcher)?;
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

fn build_artifact(
    config: &DeployConfig,
    transport: &Transport,
    launcher: &Launcher,
) -> Result<Arc<dyn ArtifactAdapter>> {
    match config.artifact.as_ref().and_then(|a| a.source.as_deref()) {
        Some("release") => Ok(Arc::new(fraisier_artifact_release::ReleaseArtifact::new())),
        // Host-pull: each host fetches + activates its own release by shelling out
        // (curl/ln) over the transport (Local single-host, Ssh per host).
        Some("pull") => Ok(Arc::new(
            fraisier_artifact_pull::PullArtifact::new().with_transport(transport.clone()),
        )),
        // Local: stage a versioned copy of an already-built local path, activate
        // via the shared atomic symlink swap (single-host / orchestrator-local).
        Some("local") => Ok(Arc::new(fraisier_artifact_local::LocalArtifact::new())),
        // Git: clone/checkout a ref into a versioned staging dir, then activate.
        Some("git") => Ok(Arc::new(fraisier_artifact_git::GitArtifact::new())),
        // IPC-over-SSH: run the rich in-process release adapter ON each host as a
        // JSON-RPC subprocess launched over ssh (Local subprocess single-host).
        Some("release-ipc") => {
            let program = config
                .artifact
                .as_ref()
                .and_then(|a| a.adapter_bin.as_deref())
                .map_or_else(
                    || std::ffi::OsString::from("fraisier-adapter-release"),
                    |path| path.as_os_str().to_owned(),
                );
            Ok(Arc::new(
                fraisier_ipc::IpcArtifactAdapter::new(program, "release")
                    .with_launcher(launcher.clone()),
            ))
        }
        Some(other) => bail!(
            "artifact source '{other}' is not available in this build \
             (built-in: 'release', 'pull', 'release-ipc', 'local', 'git')"
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

/// A ready-to-run health probe: the adapter, a base context carrying the probe
/// settings, and the hosts to probe (each `(id, optional address)`).
pub struct HealthProbePlan {
    /// The health adapter.
    pub health: Arc<dyn HealthAdapter>,
    /// The base context (probe URL + expected status in `settings`); the caller
    /// sets `host`/`settings["address"]` per host.
    pub ctx: AdapterCtx,
    /// The hosts to probe: each inventory host for a multi-host config, or a lone
    /// `localhost` for a single-host one.
    pub hosts: Vec<(HostId, Option<String>)>,
}

/// Build the health-probe plan from a config, *without* building the migration
/// adapter. The migration axis's DSN mapping is still inherited into the context
/// so the command health adapter can read `$DATABASE_URL` (the deploy path gets
/// this for free by cloning the migration ctx; a standalone `fraisier health`
/// must wire it explicitly). An http probe simply ignores the unused mapping.
///
/// # Errors
/// Fails if `[health].adapter` is missing or unsupported in this build.
pub fn build_health_probe(config: &DeployConfig) -> Result<HealthProbePlan> {
    let health = build_health(config)?;
    let deploy = config.deploy.as_ref();
    let fraise = deploy
        .and_then(|d| d.name.clone())
        .unwrap_or_else(|| "<none>".to_owned());
    let environment = deploy
        .and_then(|d| d.environment.clone())
        .unwrap_or_else(|| "<none>".to_owned());
    let mut ctx = AdapterCtx::new(fraise, environment);
    ctx.settings = settings_map(config, None);
    ctx.env_secrets = config.migration_env_secrets();
    let hosts = config.host_inventory().map_or_else(
        || vec![(HostId::new("localhost"), None)],
        |inventory| {
            inventory
                .hosts()
                .iter()
                .map(|host| (host.host.clone(), Some(host.address.clone())))
                .collect()
        },
    );
    Ok(HealthProbePlan { health, ctx, hosts })
}

/// A ready-to-run artifact probe: the adapter, a base context, and the hosts to
/// query for their live active artifact (`deployment-status --per-host`).
pub struct ArtifactProbePlan {
    /// The artifact adapter (bound to the transport/launcher for remote hosts).
    pub artifact: Arc<dyn ArtifactAdapter>,
    /// The base context (artifact settings); the caller sets `host`/`address`.
    pub ctx: AdapterCtx,
    /// The hosts to query: each inventory host, or a lone `localhost`.
    pub hosts: Vec<(HostId, Option<String>)>,
}

/// Build the artifact-probe plan, *without* touching the migration axis.
///
/// # Errors
/// Fails if `[artifact].source` is missing or unsupported in this build.
pub fn build_artifact_probe(config: &DeployConfig) -> Result<ArtifactProbePlan> {
    let transport = build_transport(config);
    let launcher = build_launcher(config);
    let artifact = build_artifact(config, &transport, &launcher)?;
    let deploy = config.deploy.as_ref();
    let fraise = deploy
        .and_then(|d| d.name.clone())
        .unwrap_or_else(|| "<none>".to_owned());
    let environment = deploy
        .and_then(|d| d.environment.clone())
        .unwrap_or_else(|| "<none>".to_owned());
    let mut ctx = AdapterCtx::new(fraise, environment);
    ctx.workdir = PathBuf::from(".");
    ctx.settings = settings_map(config, None);
    let hosts = config.host_inventory().map_or_else(
        || vec![(HostId::new("localhost"), None)],
        |inventory| {
            inventory
                .hosts()
                .iter()
                .map(|host| (host.host.clone(), Some(host.address.clone())))
                .collect()
        },
    );
    Ok(ArtifactProbePlan {
        artifact,
        ctx,
        hosts,
    })
}

/// A ready-to-run host bootstrap: the transport, a base context, the hosts to
/// prepare, and the directories each needs (`fraisier bootstrap`).
pub struct BootstrapPlan {
    /// The transport reaching each host (`Local` single-host, `Ssh` multi-host).
    pub transport: Transport,
    /// The base context (artifact settings); the caller sets `host`/`address`.
    pub ctx: AdapterCtx,
    /// The hosts to prepare: each inventory host, or a lone `localhost`.
    pub hosts: Vec<(HostId, Option<String>)>,
    /// The directories to create on each host (empty when none are configured).
    pub dirs: Vec<String>,
}

/// Build the bootstrap plan from a config: the transport, the host set, and the
/// deploy directories derived from `[artifact]` (`staging_dir` + the parent of
/// `active_path`). Touches no migration/service/health axis.
#[must_use]
pub fn build_bootstrap(config: &DeployConfig) -> BootstrapPlan {
    let transport = build_transport(config);
    let deploy = config.deploy.as_ref();
    let fraise = deploy
        .and_then(|d| d.name.clone())
        .unwrap_or_else(|| "<none>".to_owned());
    let environment = deploy
        .and_then(|d| d.environment.clone())
        .unwrap_or_else(|| "<none>".to_owned());
    let mut ctx = AdapterCtx::new(fraise, environment);
    ctx.workdir = PathBuf::from(".");
    ctx.settings = settings_map(config, None);

    let staging = config
        .artifact
        .as_ref()
        .and_then(|a| a.staging_dir.as_deref())
        .map(|p| p.to_string_lossy().into_owned());
    let active = config
        .artifact
        .as_ref()
        .and_then(|a| a.active_path.as_deref())
        .map(|p| p.to_string_lossy().into_owned());
    let dirs = fraisier_bootstrap::deploy_dirs(staging.as_deref(), active.as_deref());

    let hosts = config.host_inventory().map_or_else(
        || vec![(HostId::new("localhost"), None)],
        |inventory| {
            inventory
                .hosts()
                .iter()
                .map(|host| (host.host.clone(), Some(host.address.clone())))
                .collect()
        },
    );
    BootstrapPlan {
        transport,
        ctx,
        hosts,
        dirs,
    }
}

fn build_health(config: &DeployConfig) -> Result<Arc<dyn HealthAdapter>> {
    match config.health.as_ref().and_then(|h| h.adapter.as_deref()) {
        Some("http") => Ok(Arc::new(fraisier_adapter_http::HttpHealth::new())),
        Some("command") => Ok(Arc::new(fraisier_adapter_command::CommandHealth::new())),
        Some(other) => bail!(
            "health adapter '{other}' is not available in this build (only 'http' and 'command' are built in)"
        ),
        None => bail!("[health].adapter is required"),
    }
}

/// The migration adapter + context for a standalone database operation
/// (`db migrate`), built without the artifact/service/health axes a deploy needs.
pub struct ResolvedMigration {
    /// The fraise (deployable) name.
    pub fraise: String,
    /// The target environment.
    pub environment: String,
    /// The migration adapter (in-process or IPC).
    pub migration: Arc<dyn MigrationAdapter>,
    /// The context the adapter acts under (carries the DSN env mapping + settings).
    pub ctx: AdapterCtx,
    /// Whether to run the adapter's forward-compatibility `preflight` lint.
    pub forward_compatible_lint: bool,
}

/// Build just the migration adapter + context from a config, for a database-only
/// operation (no artifact/service/health axes resolved).
///
/// # Errors
/// Fails if `[deploy]`/`[migration]` is missing or incomplete, the adapter name
/// is unsupported in this build, or (IPC path) the configured DSN env var is unset.
pub fn build_migration_only(config: &DeployConfig) -> Result<ResolvedMigration> {
    let deploy = config.deploy.as_ref().context("missing [deploy] section")?;
    let fraise = deploy.name.clone().context("[deploy].name is required")?;
    let environment = deploy
        .environment
        .clone()
        .context("[deploy].environment is required")?;

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

    let mut ctx = AdapterCtx::new(fraise.clone(), environment.clone());
    ctx.workdir = PathBuf::from(".");
    ctx.migrations_path = config
        .migration
        .as_ref()
        .and_then(|m| m.migrations_path.clone());
    ctx.env_secrets = env_secrets;
    ctx.settings = settings_map(config, None);

    Ok(ResolvedMigration {
        fraise,
        environment,
        migration,
        ctx,
        forward_compatible_lint,
    })
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

// ---------------------------------------------------------------------------
// Blue-green (phase-07) wiring
// ---------------------------------------------------------------------------

use std::time::Duration;

use fraisier_core::adapter_axes::{TrafficDirector, TrafficTarget};
use fraisier_core::blue_green::{BlueGreenDeploy, BlueGreenParams, FleetOps};
use fraisier_core::connection_budget::{BudgetSnapshot, ConnectionBudget};

/// How often green is polled within the hold window.
const HOLD_POLL: Duration = Duration::from_secs(2);
/// Default hold window when `[blue_green].hold_secs` is unset.
const DEFAULT_HOLD_SECS: u64 = 30;
/// Default connection-budget warn margin when unset.
const DEFAULT_BUDGET_MARGIN: u32 = 10;

/// The green fleet driven via the artifact/service/health adapters, with a green
/// ctx overlay (its own unit/url/paths). Reaping shells out to `systemctl stop`
/// — the frozen `ServiceAdapter` has no `stop`, so decommissioning stays at the
/// deploy layer (consistent with the systemd substrate).
struct AdapterFleet {
    host: HostId,
    base_ctx: AdapterCtx,
    artifact: Arc<dyn ArtifactAdapter>,
    service: Arc<dyn ServiceAdapter>,
    health: Arc<dyn HealthAdapter>,
    green_unit: String,
    green_health_url: String,
    green_active_path: Option<String>,
    green_staging_dir: Option<String>,
    blue_unit: String,
    user: bool,
}

impl AdapterFleet {
    /// The adapter context for green operations: the base ctx with green's unit,
    /// health URL, and (optionally) artifact paths overlaid.
    fn green_ctx(&self) -> AdapterCtx {
        let mut ctx = self.base_ctx.clone();
        ctx.settings
            .insert("unit".to_owned(), Value::from(self.green_unit.clone()));
        ctx.settings
            .insert("url".to_owned(), Value::from(self.green_health_url.clone()));
        if let Some(path) = &self.green_active_path {
            ctx.settings
                .insert("active_path".to_owned(), Value::from(path.clone()));
        }
        if let Some(dir) = &self.green_staging_dir {
            ctx.settings
                .insert("staging_dir".to_owned(), Value::from(dir.clone()));
        }
        ctx
    }

    async fn stop_unit(&self, unit: &str) -> Result<(), String> {
        let mut command = tokio::process::Command::new("systemctl");
        if self.user {
            command.arg("--user");
        }
        command.arg("stop").arg(unit);
        let output = command
            .output()
            .await
            .map_err(|e| format!("spawning `systemctl stop {unit}`: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }
}

#[async_trait::async_trait]
impl FleetOps for AdapterFleet {
    async fn provision_green(&self, _ctx: &AdapterCtx) -> Result<(), String> {
        let ctx = self.green_ctx();
        let staged = self
            .artifact
            .stage(&ctx, &self.host)
            .await
            .map_err(|e| e.to_string())?;
        self.artifact
            .activate(&ctx, &self.host, &staged)
            .await
            .map_err(|e| e.to_string())?;
        self.service
            .restart(&ctx, &self.host)
            .await
            .map_err(|e| e.to_string())
    }

    async fn green_healthy(&self, _ctx: &AdapterCtx) -> bool {
        self.health
            .check(&self.green_ctx(), &self.host)
            .await
            .is_ok_and(|status| status.healthy)
    }

    async fn watch_green(&self, ctx: &AdapterCtx, hold: Duration) -> Result<(), String> {
        let mut elapsed = Duration::ZERO;
        while elapsed < hold {
            if !self.green_healthy(ctx).await {
                return Err("green degraded during the hold window".to_owned());
            }
            tokio::time::sleep(HOLD_POLL).await;
            elapsed += HOLD_POLL;
        }
        Ok(())
    }

    async fn reap_green(&self, _ctx: &AdapterCtx) -> Result<(), String> {
        self.stop_unit(&self.green_unit).await
    }

    async fn reap_blue(&self, _ctx: &AdapterCtx) -> Result<(), String> {
        self.stop_unit(&self.blue_unit).await
    }
}

/// A connection-budget probe that queries the shared Postgres via `psql` (PG* env
/// from the migration DSN — never argv).
struct PgConnectionBudget {
    dsn_env: String,
}

impl PgConnectionBudget {
    async fn query_u32(conn: &fraisier_db::PgConn, sql: &str) -> Result<u32, String> {
        let mut command = tokio::process::Command::from(fraisier_db::psql_command(conn, sql));
        command.arg("-tA"); // tuples-only, unaligned → just the value
        let output = command
            .output()
            .await
            .map_err(|e| format!("spawning psql: {e}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|e| format!("parsing `{sql}` result: {e}"))
    }
}

#[async_trait::async_trait]
impl ConnectionBudget for PgConnectionBudget {
    async fn probe(&self, _ctx: &AdapterCtx) -> Result<BudgetSnapshot, String> {
        let dsn =
            std::env::var(&self.dsn_env).map_err(|_| format!("{} is not set", self.dsn_env))?;
        let conn = fraisier_db::PgConn::parse(&dsn).map_err(|e| e.to_string())?;
        let max_connections = Self::query_u32(&conn, "SHOW max_connections").await?;
        let current = Self::query_u32(&conn, "SELECT count(*) FROM pg_stat_activity").await?;
        Ok(BudgetSnapshot {
            max_connections,
            current,
        })
    }
}

/// The fully-built blue-green deploy + its identity, for the CLI dispatch.
pub struct ResolvedBlueGreen {
    /// The fraise (deployable) name.
    pub fraise: String,
    /// The target environment.
    pub environment: String,
    /// The composed blue-green deploy, ready to `run`.
    pub deploy: BlueGreenDeploy,
}

/// Build a **blue-green** deploy from a `[deploy].strategy = "blue-green"` config:
/// the green fleet over the artifact/service/health adapters, the built-in nginx
/// [`TrafficDirector`], the window-safety gate's migration adapter, and (when a
/// DSN is configured) the connection-budget probe.
///
/// # Errors
/// Fails if a required section/field is missing or an adapter name is unsupported.
pub fn build_blue_green(
    config: &DeployConfig,
    app_version: Option<&str>,
) -> Result<ResolvedBlueGreen> {
    let deploy = config.deploy.as_ref().context("missing [deploy] section")?;
    let fraise = deploy.name.clone().context("[deploy].name is required")?;
    let environment = deploy
        .environment
        .clone()
        .context("[deploy].environment is required")?;
    let bg = config
        .blue_green
        .as_ref()
        .context("[deploy].strategy = \"blue-green\" requires a [blue_green] section")?;

    let host = resolve_host(config, None)?;
    let transport = build_transport(config);
    let launcher = build_launcher(config);
    let artifact = build_artifact(config, &transport, &launcher)?;
    let service = build_service(config, &transport)?;
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
    ctx.settings = settings_map(config, app_version);

    let blue_unit = config
        .service
        .as_ref()
        .and_then(|s| s.unit.clone())
        .context("blue-green requires [service].unit (the blue fleet's unit)")?;
    let user = config
        .service
        .as_ref()
        .and_then(|s| s.user)
        .unwrap_or(false);

    let fleet: Arc<dyn FleetOps> = Arc::new(AdapterFleet {
        host,
        base_ctx: ctx.clone(),
        artifact,
        service,
        health,
        green_unit: bg
            .green_unit
            .clone()
            .context("[blue_green].green_unit is required")?,
        green_health_url: bg
            .green_health_url
            .clone()
            .context("[blue_green].green_health_url is required")?,
        green_active_path: bg
            .green_active_path
            .as_ref()
            .map(|p| p.display().to_string()),
        green_staging_dir: bg
            .green_staging_dir
            .as_ref()
            .map(|p| p.display().to_string()),
        blue_unit,
        user,
    });

    let traffic: Arc<dyn TrafficDirector> = Arc::new(fraisier_adapter_nginx::NginxLb::new());

    // The connection-budget probe is wired only when a DSN env + green_pool are
    // configured; otherwise the check is skipped (None).
    let budget: Option<Arc<dyn ConnectionBudget>> = match (&database_url_env, bg.green_pool) {
        (Some(dsn_env), Some(_)) => Some(Arc::new(PgConnectionBudget {
            dsn_env: dsn_env.clone(),
        })),
        _ => None,
    };

    let params = BlueGreenParams {
        fraise: fraise.clone(),
        environment: environment.clone(),
        ctx,
        green: TrafficTarget::new(bg.green.as_deref().unwrap_or("green")),
        hold: Duration::from_secs(bg.hold_secs.unwrap_or(DEFAULT_HOLD_SECS)),
        green_pool: bg.green_pool.unwrap_or(0),
        budget_margin: bg.connection_margin.unwrap_or(DEFAULT_BUDGET_MARGIN),
    };

    Ok(ResolvedBlueGreen {
        fraise,
        environment,
        deploy: BlueGreenDeploy::new(params, migration, traffic, fleet, budget),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build, build_health, build_health_probe, build_multi_host, summarize, summarize_multi_host,
    };
    use fraisier_config::DeployConfig;

    /// A single-host config whose `[health]` block is command-driven, with the
    /// given extra health lines appended (e.g. `timeout_ms = …`).
    fn single_host_command_health(extra_health: &str) -> String {
        format!(
            "[deploy]\nname = \"app\"\nenvironment = \"prod\"\n\n\
             [artifact]\nsource = \"local\"\npath = \"/builds/app\"\nactive_path = \"/srv/app/current\"\n\n\
             [migration]\nadapter = \"confiture\"\ndatabase_url_env = \"APP_DATABASE_URL\"\n\n\
             [service]\nadapter = \"systemd\"\nunit = \"app.service\"\n\n\
             [health]\nadapter = \"command\"\ncommand = \"scan\"\n{extra_health}"
        )
    }

    #[test]
    fn command_health_adapter_builds_and_plumbs_settings_and_dsn() {
        let toml = single_host_command_health("timeout_ms = 30000\n");
        let config = DeployConfig::from_toml_str(&toml).expect("parses");

        // The `command` arm yields an adapter.
        assert!(build_health(&config).is_ok(), "command health builds");

        // `command` + `timeout_ms` reach ctx.settings (read by CommandHealth).
        let plan = build_health_probe(&config).expect("builds command health probe");
        assert_eq!(
            plan.ctx
                .settings
                .get("command")
                .and_then(serde_json::Value::as_str),
            Some("scan"),
        );
        assert_eq!(
            plan.ctx
                .settings
                .get("timeout_ms")
                .and_then(serde_json::Value::as_u64),
            Some(30_000),
        );

        // Standalone-health gap fix: the DSN mapping is inherited so the command
        // can read $DATABASE_URL, exactly as the deploy-path health step does.
        assert_eq!(
            plan.ctx.env_secrets.get("DATABASE_URL").map(String::as_str),
            Some("APP_DATABASE_URL"),
        );
    }

    #[test]
    fn command_health_renders_in_the_summary() {
        let toml = single_host_command_health("");
        let config = DeployConfig::from_toml_str(&toml).expect("parses");
        let summary = summarize(&config, Some("localhost"), None).expect("summarize");
        assert_eq!(summary.health, "command");
        assert!(
            summary.settings_keys.iter().any(|k| k == "command"),
            "command is in the assembled settings: {:?}",
            summary.settings_keys,
        );
    }

    #[test]
    fn unknown_health_adapter_bails() {
        let toml = single_host_command_health("").replace(
            "[health]\nadapter = \"command\"\ncommand = \"scan\"\n",
            "[health]\nadapter = \"telepathy\"\n",
        );
        let config = DeployConfig::from_toml_str(&toml).expect("parses");
        // `.err()` drops the `Arc<dyn HealthAdapter>` Ok value (which isn't Debug).
        let err = build_health(&config)
            .err()
            .expect("unknown health adapter bails");
        assert!(
            err.to_string().contains("telepathy"),
            "error names the adapter: {err}",
        );
    }

    /// A single-host config with the given artifact source block.
    fn single_host_with_artifact(artifact: &str) -> String {
        format!(
            "[deploy]\nname = \"app\"\nenvironment = \"prod\"\n\n{artifact}\n\n\
             [migration]\nadapter = \"command\"\n\n\
             [service]\nadapter = \"systemd\"\nunit = \"app.service\"\n\n\
             [health]\nadapter = \"http\"\nurl = \"http://127.0.0.1:8080/health\"\n"
        )
    }

    #[test]
    fn local_and_git_artifact_sources_build() {
        // local: an already-built path on disk.
        let local = single_host_with_artifact(
            "[artifact]\nsource = \"local\"\npath = \"/builds/app\"\nactive_path = \"/srv/app/current\"",
        );
        let config = DeployConfig::from_toml_str(&local).expect("parses");
        assert!(build(&config, Some("localhost"), Some("1.0.0")).is_ok());

        // git: a repo + ref.
        let git = single_host_with_artifact(
            "[artifact]\nsource = \"git\"\nrepo = \"https://x/app.git\"\nref = \"v1\"\nactive_path = \"/srv/app/current\"",
        );
        let config = DeployConfig::from_toml_str(&git).expect("parses");
        assert!(build(&config, Some("localhost"), Some("1.0.0")).is_ok());
    }

    /// A complete multi-host config selecting the IPC-over-SSH artifact source.
    const MULTI_HOST_IPC: &str = r#"
[deploy]
name = "app"
environment = "prod"

[hosts]
strategy = "rolling"
rolling_batch_size = 1
inventory = [ { name = "web-1", address = "10.0.0.1" } ]

[ssh]
user = "deploy"

[artifact]
source = "release-ipc"
release_url = "https://x/app-{version}.tar.gz"
checksum = "abc"
active_path = "/srv/app/current"
adapter_bin = "/usr/local/bin/fraisier-adapter-release"

[migration]
adapter = "command"

[service]
adapter = "systemd"
unit = "app.service"

[health]
adapter = "http"
url = "http://{host.address}:8080/health"

[lb]
adapter = "nginx"
config_path = "/etc/nginx/nginx.conf"
"#;

    /// `source = "release-ipc"` builds an artifact adapter for a multi-host deploy
    /// and the summary renders it over an ssh launcher.
    #[test]
    fn release_ipc_artifact_builds_for_multi_host() {
        let config = DeployConfig::from_toml_str(MULTI_HOST_IPC).expect("parses");
        // Building constructs the IpcArtifactAdapter over the ssh launcher; a
        // missing/typo'd source would `bail!`.
        let _ = build_multi_host(&config, Some("1.2.3")).expect("builds multi-host");

        let summary = summarize_multi_host(&config, Some("1.2.3")).expect("summary");
        assert_eq!(summary.artifact, "release-ipc");
        assert!(
            summary.transport.starts_with("ssh"),
            "{}",
            summary.transport
        );
    }

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

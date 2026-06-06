//! The validation pass (PRD §7.1, Cycle 1.12 REFACTOR).
//!
//! Validation is deliberately a **separate pass** from parsing: a
//! [`DeployConfig`] always parses into a structurally-valid value (every section
//! optional), and `validate` then judges it against the value-domain and
//! per-adapter requirements, collecting *every* issue so the operator sees them
//! all at once. It performs **no I/O** — filesystem and network checks belong at
//! deploy time, not config-load time.

use std::collections::BTreeSet;

use fraisier_core::adapter_axes::Severity;
use serde::{Deserialize, Serialize};

use crate::schema::{STRATEGY_ALL_AT_ONCE, STRATEGY_ROLLING};
use crate::DeployConfig;

/// The lowest and highest meaningful HTTP status codes.
const HTTP_STATUS_RANGE: std::ops::RangeInclusive<u16> = 100..=599;

/// One finding from [`DeployConfig::validate`], located at a `section.field`
/// path.
///
/// # Example
/// ```
/// use fraisier_config::{DeployConfig, Severity};
///
/// let cfg = DeployConfig::from_toml_str("[migration]\nadapter = \"confiture\"\n")
///     .expect("parses");
/// let report = cfg.validate();
/// assert!(report.issues.iter().any(|i| i.severity == Severity::Error));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// How serious the finding is. An [`Severity::Error`] makes the config
    /// invalid; [`Severity::Warning`] / [`Severity::Info`] do not.
    pub severity: Severity,
    /// The located path, e.g. `"migration.database_url_env"`.
    pub path: String,
    /// A human-readable, actionable description.
    pub message: String,
}

/// The result of [`DeployConfig::validate`]: the full list of findings.
///
/// The config is acceptable iff [`ValidationReport::ok`] (no `Error`-severity
/// issues); any warnings are still surfaced for the operator.
///
/// # Example
/// ```
/// use fraisier_config::DeployConfig;
///
/// let cfg = DeployConfig::from_toml_str(
///     "[deploy]\nname = \"app\"\nenvironment = \"prod\"\n",
/// )
/// .expect("parses");
/// let report = cfg.validate();
/// // Missing required sections → not ok.
/// assert!(!report.ok());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// Every finding, in section order.
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    /// Whether the config is acceptable: no `Error`-severity issues.
    #[must_use]
    pub fn ok(&self) -> bool {
        !self.issues.iter().any(|i| i.severity == Severity::Error)
    }

    /// The `Error`-severity issues only.
    pub fn errors(&self) -> impl Iterator<Item = &ValidationIssue> {
        self.issues.iter().filter(|i| i.severity == Severity::Error)
    }

    /// The non-error issues (warnings and info).
    pub fn warnings(&self) -> impl Iterator<Item = &ValidationIssue> {
        self.issues.iter().filter(|i| i.severity != Severity::Error)
    }

    fn push(&mut self, severity: Severity, path: &str, message: impl Into<String>) {
        self.issues.push(ValidationIssue {
            severity,
            path: path.to_owned(),
            message: message.into(),
        });
    }

    fn error(&mut self, path: &str, message: impl Into<String>) {
        self.push(Severity::Error, path, message);
    }

    fn warn(&mut self, path: &str, message: impl Into<String>) {
        self.push(Severity::Warning, path, message);
    }
}

impl std::fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for issue in &self.issues {
            let tag = match issue.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            writeln!(f, "  [{tag}] {}: {}", issue.path, issue.message)?;
        }
        Ok(())
    }
}

impl DeployConfig {
    /// Validate the config, returning every located issue (Cycle 1.12).
    ///
    /// This is pure: it performs no filesystem or network access. Call it after
    /// [`from_toml_str`](DeployConfig::from_toml_str); [`load`](DeployConfig::load)
    /// runs it for you and fails on any `Error`-severity issue.
    #[must_use]
    pub fn validate(&self) -> ValidationReport {
        let mut report = ValidationReport::default();
        validate_deploy(self, &mut report);
        validate_artifact(self, &mut report);
        validate_migration(self, &mut report);
        validate_service(self, &mut report);
        validate_health(self, &mut report);
        validate_hosts(self, &mut report);
        validate_lb(self, &mut report);
        validate_webhook(self, &mut report);
        validate_schedule(self, &mut report);
        validate_sync(self, &mut report);
        validate_blue_green(self, &mut report);
        report
    }

    /// Validate only the sections a database operation needs.
    ///
    /// The `db migrate|backup|restore|reset` commands act on the database alone,
    /// so they require `[deploy]` (for the state-ledger identity) and
    /// `[migration]` (the adapter + DSN source), but **not** the
    /// artifact/service/health/lb axes a full deploy needs. This is the same pure,
    /// no-I/O check restricted to those two sections.
    #[must_use]
    pub fn validate_db_ops(&self) -> ValidationReport {
        let mut report = ValidationReport::default();
        validate_deploy(self, &mut report);
        validate_migration(self, &mut report);
        report
    }
}

/// Whether an optional string field is present and non-blank.
fn is_set(field: Option<&String>) -> bool {
    field.is_some_and(|s| !s.trim().is_empty())
}

fn validate_deploy(cfg: &DeployConfig, report: &mut ValidationReport) {
    let Some(deploy) = &cfg.deploy else {
        report.error("deploy", "missing required [deploy] section");
        return;
    };
    if !is_set(deploy.name.as_ref()) {
        report.error(
            "deploy.name",
            "[deploy].name is required and must be non-empty \
             (the [specql] preset supplies it from its `name` field)",
        );
    }
    if !is_set(deploy.environment.as_ref()) {
        report.error(
            "deploy.environment",
            "[deploy].environment is required and must be non-empty",
        );
    }
}

fn validate_artifact(cfg: &DeployConfig, report: &mut ValidationReport) {
    let Some(artifact) = &cfg.artifact else {
        report.error("artifact", "missing required [artifact] section");
        return;
    };
    match artifact.source.as_deref() {
        None => report.error(
            "artifact.source",
            "[artifact].source is required (one of: release, pull, git, local)",
        ),
        // `release` (orchestrator HTTP fetch), `pull` (host fetches via curl),
        // and `release-ipc` (host runs the release adapter over ssh/IPC) take the
        // same URL + checksum fields.
        Some(source @ ("release" | "pull" | "release-ipc")) => {
            if !is_set(artifact.release_url.as_ref()) {
                report.error(
                    "artifact.release_url",
                    format!("the {source} source requires release_url"),
                );
            }
            if !is_set(artifact.checksum_url.as_ref()) && !is_set(artifact.checksum.as_ref()) {
                report.error(
                    "artifact.checksum_url",
                    format!(
                        "the {source} source requires checksum_url (or an inline checksum) \
                         so the download can be sha256-verified before it is staged"
                    ),
                );
            }
        }
        Some("git") => {
            if !is_set(artifact.repo.as_ref()) {
                report.error("artifact.repo", "the git source requires repo");
            }
        }
        Some("local") => {
            if artifact.path.is_none() {
                report.error("artifact.path", "the local source requires path");
            }
        }
        Some(other) => report.error(
            "artifact.source",
            format!(
                "unknown artifact source '{other}' \
                 (expected one of: release, pull, release-ipc, git, local)"
            ),
        ),
    }
    // `active_path` is what activation swaps to the new artifact; a deploy that
    // activates one (release/local/release-ipc) cannot complete without it. Warn
    // rather than error so the PRD example (which omits it) still validates; the
    // artifact adapter hard-errors at activate time.
    if matches!(
        artifact.source.as_deref(),
        Some("release" | "local" | "release-ipc")
    ) && artifact.active_path.is_none()
    {
        report.warn(
            "artifact.active_path",
            "no active_path set; activation will fail at deploy time \
             (it is the symlink swapped to the newly-staged artifact)",
        );
    }
}

fn validate_migration(cfg: &DeployConfig, report: &mut ValidationReport) {
    let Some(migration) = &cfg.migration else {
        report.error("migration", "missing required [migration] section");
        return;
    };
    let adapter = migration.adapter.as_deref();
    if !is_set(migration.adapter.as_ref()) {
        report.error("migration.adapter", "[migration].adapter is required");
    }
    // A database-backed adapter needs a DSN; the only way to supply one is the
    // env-var indirection (Decision 5).
    if adapter == Some("confiture") && !is_set(migration.database_url_env.as_ref()) {
        report.error(
            "migration.database_url_env",
            "the confiture adapter requires database_url_env \
             (the name of the env var holding the database DSN)",
        );
    }
    if migration.forward_compatible_lint == Some(true) && adapter == Some("command") {
        report.warn(
            "migration.forward_compatible_lint",
            "the command adapter does not implement preflight; \
             forward_compatible_lint will be skipped",
        );
    }
}

fn validate_service(cfg: &DeployConfig, report: &mut ValidationReport) {
    let Some(service) = &cfg.service else {
        report.error("service", "missing required [service] section");
        return;
    };
    if !is_set(service.adapter.as_ref()) {
        report.error("service.adapter", "[service].adapter is required");
    }
    if service.adapter.as_deref() == Some("systemd") && !is_set(service.unit.as_ref()) {
        report.error(
            "service.unit",
            "the systemd adapter requires unit (the .service unit name)",
        );
    }
    if service.adapter.as_deref() == Some("rc") && !is_set(service.name.as_ref()) {
        report.error(
            "service.name",
            "the rc adapter requires name (the rc.d service name)",
        );
    }
    if service.adapter.as_deref() == Some("docker-compose")
        && !is_set(service.compose_service.as_ref())
    {
        report.error(
            "service.compose_service",
            "the docker-compose adapter requires compose_service \
             (the service within the Compose project)",
        );
    }
    if service.user == Some(true) && service.adapter.as_deref() != Some("systemd") {
        report.warn(
            "service.user",
            "user = true only applies to the systemd adapter (systemctl --user); \
             it is ignored by this adapter",
        );
    }
}

fn validate_health(cfg: &DeployConfig, report: &mut ValidationReport) {
    let Some(health) = &cfg.health else {
        report.error("health", "missing required [health] section");
        return;
    };
    if !is_set(health.adapter.as_ref()) {
        report.error("health.adapter", "[health].adapter is required");
    }
    if health.adapter.as_deref() == Some("http") && !is_set(health.url.as_ref()) {
        report.error("health.url", "the http adapter requires url");
    }
    if let Some(status) = health.expected_status {
        if !HTTP_STATUS_RANGE.contains(&status) {
            report.error(
                "health.expected_status",
                format!("expected_status {status} is not a valid HTTP status (100–599)"),
            );
        }
    }
}

fn validate_hosts(cfg: &DeployConfig, report: &mut ValidationReport) {
    let Some(hosts) = &cfg.hosts else {
        return; // single-host: [hosts] is legitimately absent
    };
    if hosts.inventory.is_empty() {
        report.error(
            "hosts.inventory",
            "[hosts] is present but its inventory is empty",
        );
    }

    let mut seen = BTreeSet::new();
    for (index, host) in hosts.inventory.iter().enumerate() {
        if !is_set(host.name.as_ref()) {
            report.error(
                &format!("hosts.inventory[{index}].name"),
                "each inventory host requires a name",
            );
        }
        if !is_set(host.address.as_ref()) {
            report.error(
                &format!("hosts.inventory[{index}].address"),
                "each inventory host requires an address",
            );
        }
        if let Some(name) = &host.name {
            if !seen.insert(name.clone()) {
                report.error(
                    "hosts.inventory",
                    format!("duplicate host name '{name}' in the inventory"),
                );
            }
        }
    }

    match hosts.strategy.as_deref() {
        None | Some(STRATEGY_ROLLING) => validate_rolling(hosts, report),
        Some(s) if STRATEGY_ALL_AT_ONCE.contains(&s) => {}
        Some(other) => report.error(
            "hosts.strategy",
            format!("unknown strategy '{other}' (expected: rolling, all-at-once)"),
        ),
    }

    warn_local_only_axes(cfg, report);
}

/// In a multi-host deploy the service axis runs on each host over SSH. The
/// `release`/`local`/`git` artifact adapters and the `nginx` LB adapter still do
/// local-filesystem work, so warn the operator they act where fraisier runs. The
/// genuinely-remote artifact sources (`pull` via curl, `release-ipc` via the
/// adapter over ssh) stage on each host and are not warned.
fn warn_local_only_axes(cfg: &DeployConfig, report: &mut ValidationReport) {
    if let Some(source) = cfg.artifact.as_ref().and_then(|a| a.source.as_deref()) {
        if matches!(source, "release" | "local" | "git") {
            report.warn(
                "artifact.source",
                format!(
                    "multi-host: the '{source}' artifact adapter stages on the host running \
                     fraisier, not on each remote host — use source = \"pull\" (curl over SSH) \
                     or source = \"release-ipc\" (the release adapter run on each host over SSH) \
                     to stage + activate per host"
                ),
            );
        }
    }
    if cfg.lb.as_ref().and_then(|lb| lb.adapter.as_deref()) == Some("nginx") {
        report.warn(
            "lb.adapter",
            "multi-host: the nginx adapter edits its config where fraisier runs, so the load \
             balancer must run on that host (a remote-LB topology is not yet wired)",
        );
    }
}

fn validate_rolling(hosts: &crate::schema::HostsSection, report: &mut ValidationReport) {
    match hosts.rolling_batch_size {
        Some(0) => report.error(
            "hosts.rolling_batch_size",
            "rolling_batch_size must be at least 1",
        ),
        Some(n) if !hosts.inventory.is_empty() && n > hosts.inventory.len() => report.warn(
            "hosts.rolling_batch_size",
            format!(
                "rolling_batch_size {n} exceeds the {} hosts in the inventory; \
                 the rollout behaves like all-at-once",
                hosts.inventory.len()
            ),
        ),
        _ => {}
    }
}

fn validate_webhook(cfg: &DeployConfig, report: &mut ValidationReport) {
    let Some(webhook) = &cfg.webhook else {
        return; // optional
    };
    if !is_set(webhook.secret_env.as_ref()) {
        report.error(
            "webhook.secret_env",
            "the [webhook] server requires secret_env \
             (the name of the env var holding the shared HMAC secret)",
        );
    }
}

/// Recognised `[schedule].command` values (what the timer runs).
const SCHEDULE_COMMANDS: [&str; 2] = ["deploy", "backup"];

fn validate_sync(cfg: &DeployConfig, report: &mut ValidationReport) {
    let Some(sync) = &cfg.sync else {
        return; // optional
    };
    if !is_set(sync.remote.as_ref()) {
        report.error(
            "sync.remote",
            "the [sync] ledger requires remote (the git remote it is shared through)",
        );
    }
}

fn validate_schedule(cfg: &DeployConfig, report: &mut ValidationReport) {
    let Some(schedule) = &cfg.schedule else {
        return; // optional
    };

    // Exactly one calendar surface: the stable native vocab, or the raw escape
    // hatch — never both, never neither.
    let has_calendar = is_set(schedule.calendar.as_ref());
    let has_raw = is_set(schedule.on_calendar_raw.as_ref());
    match (has_calendar, has_raw) {
        (false, false) => report.error(
            "schedule.calendar",
            "the [schedule] timer requires `calendar` (e.g. \"daily 03:00\") \
             or the systemd-coupled escape hatch `on_calendar_raw`",
        ),
        (true, true) => report.error(
            "schedule.calendar",
            "set either `calendar` or `on_calendar_raw`, not both",
        ),
        _ => {
            if let Some(spec) = schedule.calendar.as_deref() {
                if let Err(message) = crate::calendar::to_on_calendar(spec) {
                    report.error("schedule.calendar", message);
                }
            }
        }
    }

    // `command` is explicit (no silent default): an unattended deploy is never an
    // accident.
    match schedule.command.as_deref() {
        None => report.error(
            "schedule.command",
            "set `command` explicitly (\"deploy\" or \"backup\") — there is no default",
        ),
        Some(command) if !SCHEDULE_COMMANDS.contains(&command) => report.error(
            "schedule.command",
            format!("unknown schedule command '{command}' (expected: deploy, backup)"),
        ),
        Some("deploy") => {
            // The unattended-deploy gate: opt-in + a notify sink, both required.
            if schedule.allow_unattended_deploy != Some(true) {
                report.error(
                    "schedule.allow_unattended_deploy",
                    "an unattended scheduled deploy requires allow_unattended_deploy = true \
                     (it removes the operator-is-watching protection by construction)",
                );
            }
            if !is_set(schedule.notify.as_ref()) {
                report.error(
                    "schedule.notify",
                    "an unattended scheduled deploy requires a `notify` failure sink",
                );
            }
        }
        Some(_) => {}
    }
}

fn validate_blue_green(cfg: &DeployConfig, report: &mut ValidationReport) {
    match cfg.deploy.as_ref().and_then(|d| d.strategy.as_deref()) {
        None => return, // default single/rolling behavior
        Some("blue-green") => {}
        Some(other) => {
            report.error(
                "deploy.strategy",
                format!("unknown deploy strategy '{other}' (expected: blue-green)"),
            );
            return;
        }
    }

    // Blue-green needs the green fleet's coordinates, the nginx traffic director,
    // and a migration adapter (for the window-safety gate).
    let Some(bg) = &cfg.blue_green else {
        report.error(
            "blue_green",
            "[deploy].strategy = \"blue-green\" requires a [blue_green] section",
        );
        return;
    };
    if !is_set(bg.green_unit.as_ref()) {
        report.error(
            "blue_green.green_unit",
            "blue-green requires green_unit (the green fleet's service unit)",
        );
    }
    if !is_set(bg.green_health_url.as_ref()) {
        report.error(
            "blue_green.green_health_url",
            "blue-green requires green_health_url (green's pre-swap health gate)",
        );
    }
    if bg.green_servers.is_empty() {
        report.error(
            "blue_green.green_servers",
            "blue-green requires green_servers (green's nginx upstream backends)",
        );
    }
    match &cfg.lb {
        Some(lb) if lb.adapter.as_deref() == Some("nginx") => {
            if !is_set(lb.upstream.as_ref()) {
                report.error("lb.upstream", "blue-green requires [lb].upstream");
            }
            if lb.include_dir.is_none() {
                report.error(
                    "lb.include_dir",
                    "blue-green requires [lb].include_dir (the traffic-swap include dir)",
                );
            }
        }
        _ => report.error(
            "lb.adapter",
            "blue-green requires an [lb] section with adapter = \"nginx\" (the traffic director)",
        ),
    }
    if cfg
        .migration
        .as_ref()
        .is_none_or(|m| !is_set(m.adapter.as_ref()))
    {
        report.error(
            "migration.adapter",
            "blue-green requires a [migration] adapter for the window-safety gate",
        );
    }
}

fn validate_lb(cfg: &DeployConfig, report: &mut ValidationReport) {
    let Some(lb) = &cfg.lb else {
        return; // optional
    };
    if !is_set(lb.adapter.as_ref()) {
        report.error("lb.adapter", "[lb].adapter is required");
    }
    if lb.adapter.as_deref() == Some("nginx") {
        // Rolling drains rewrite `config_path`; blue-green swaps `include_dir`.
        // One of them is required (the strategy-specific validator pins which).
        if lb.config_path.is_none() && lb.include_dir.is_none() {
            report.error(
                "lb.config_path",
                "the nginx adapter requires config_path (rolling) or include_dir (blue-green)",
            );
        }
        if !is_set(lb.upstream.as_ref()) {
            report.error("lb.upstream", "the nginx adapter requires upstream");
        }
    }
}

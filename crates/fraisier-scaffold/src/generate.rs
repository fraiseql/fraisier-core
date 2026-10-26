//! Rendering the deploy infrastructure files from a [`DeployConfig`].
//!
//! Each generated file carries the [`MARKER`] header so the installer can later
//! recognise fraisier's own files and prune stale ones safely. Templates are
//! plain `{placeholder}` substitution (rather than a templating engine) — the
//! files are static skeletons with a handful of fields, and GitHub Actions'
//! `${{ … }}` syntax would otherwise collide with Jinja-style delimiters.

// Reason: the template consts intentionally embed `{name}`-style placeholders we
// resolve with `str::replace`, not `format!`; this lint reads them as stray
// format args, which is exactly the idiom here.
#![allow(clippy::literal_string_with_formatting_args)]

use std::path::PathBuf;

use fraisier_config::{DeployConfig, RetainEntry};

use crate::ScaffoldError;

/// The header line every generated file starts with. Install/prune treat the
/// presence of this marker as "fraisier owns this file".
pub const MARKER: &str =
    "fraisier-generated: do not edit by hand (regenerate with `fraisier scaffold`)";

/// One file produced by [`generate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    /// Path within the scaffold output tree (e.g. `systemd/app.service`).
    pub rel_path: PathBuf,
    /// Absolute system path the installer copies this file to, or `None` for
    /// files that belong in the repository (CI workflow, `.env.example`).
    pub install_dest: Option<PathBuf>,
    /// The rendered file contents, marker header included.
    pub contents: String,
}

/// Render the full set of deploy infrastructure files for `config`.
///
/// Produces the systemd service + socket units, an nginx site config, a GitHub
/// Actions deploy workflow, and a `.env.example`.
///
/// # Errors
/// [`ScaffoldError::MissingField`] if `[deploy].name` / `[deploy].environment`
/// (the fields every template keys on) are absent.
pub fn generate(config: &DeployConfig) -> Result<Vec<GeneratedFile>, ScaffoldError> {
    let r = Resolved::from_config(config)?;
    let service = with_header(
        &SYSTEMD_SERVICE
            .replace("{name}", &r.name)
            .replace("{environment}", &r.environment)
            .replace("{exec_start}", &r.exec_start)
            .replace("{env_file}", &r.env_file),
    );
    let socket = with_header(
        &SYSTEMD_SOCKET
            .replace("{name}", &r.name)
            .replace("{environment}", &r.environment),
    );
    let nginx = with_header(
        &NGINX_SITE
            .replace("{server_block}", &r.server_block)
            .replace("{upstream}", &r.upstream)
            .replace("{name}", &r.name),
    );
    let ci = with_header(
        &CI_WORKFLOW
            .replace("{name}", &r.name)
            .replace("{environment}", &r.environment),
    );
    let env = with_header(
        &ENV_EXAMPLE
            .replace("{db_env}", &r.db_env)
            .replace("{env_file}", &r.env_file)
            .replace("{name}", &r.name)
            .replace("{environment}", &r.environment),
    );

    Ok(vec![
        GeneratedFile {
            rel_path: PathBuf::from(format!("systemd/{}", r.unit)),
            install_dest: Some(PathBuf::from(format!("/etc/systemd/system/{}", r.unit))),
            contents: service,
        },
        GeneratedFile {
            rel_path: PathBuf::from(format!("systemd/{}", r.socket)),
            install_dest: Some(PathBuf::from(format!("/etc/systemd/system/{}", r.socket))),
            contents: socket,
        },
        GeneratedFile {
            rel_path: PathBuf::from(format!("nginx/{}.conf", r.name)),
            install_dest: Some(r.nginx_dest),
            contents: nginx,
        },
        GeneratedFile {
            rel_path: PathBuf::from(".github/workflows/deploy.yml"),
            install_dest: None,
            contents: ci,
        },
        GeneratedFile {
            rel_path: PathBuf::from(".env.example"),
            install_dest: None,
            contents: env,
        },
    ])
}

/// Render the systemd units that run fraisier on a calendar schedule.
///
/// - `[schedule]`: `fraisier-<name>-<environment>-scheduled.{service,timer}`, a
///   oneshot service running `fraisier <command> --config <config_path>`.
///   `<command>` is explicit (no default); `<config_path>` defaults to
///   `/etc/fraisier/<name>.toml`. The `OnCalendar=` comes from the native
///   `calendar` vocabulary (translated) or the `on_calendar_raw` escape hatch.
/// - each `[[backup.retain]]` entry: `…-retain-<entry>.{service,timer}`,
///   running `fraisier backup prune --name <entry> --apply`;
/// - a `[drill]` with a `schedule`: `…-drill.{service,timer}`.
///
/// A scheduled backup, a prune and a drill each name
/// `fraisier-<name>-<environment>-alert@.service` in `OnFailure=` and run with
/// `--failure-record %n`; that alert template is generated with them, because
/// an `OnFailure=` naming a missing unit fails silently. A scheduled deploy
/// notifies in-process instead and references no alert.
///
/// # Errors
/// [`ScaffoldError::MissingField`] if `[deploy].name`/`[deploy].environment`
/// is absent, if nothing is scheduled, or if `[schedule]` lacks its `command`
/// or a calendar surface; [`ScaffoldError::InvalidField`] if a calendar is
/// malformed or a retain entry has no usable name.
pub fn generate_scheduled(config: &DeployConfig) -> Result<Vec<GeneratedFile>, ScaffoldError> {
    let deploy = config.deploy.as_ref();
    let name = required(deploy.and_then(|d| d.name.as_deref()), "deploy", "name")?;
    let environment = required(
        deploy.and_then(|d| d.environment.as_deref()),
        "deploy",
        "environment",
    )?;
    let schedule = config.schedule.as_ref();
    let retain = config
        .backup
        .as_ref()
        .map_or(&[][..], |backup| &backup.retain[..]);
    let drill_schedule = config
        .drill
        .as_ref()
        .and_then(|drill| drill.schedule.as_deref())
        .filter(|spec| !spec.trim().is_empty());
    if schedule.is_none() && retain.is_empty() && drill_schedule.is_none() {
        return Err(ScaffoldError::MissingField {
            section: "schedule",
            field: "calendar",
        });
    }

    let config_path = schedule.and_then(|s| s.config_path.as_deref()).map_or_else(
        || format!("/etc/fraisier/{name}.toml"),
        |p| p.display().to_string(),
    );
    let units = Units {
        name: &name,
        environment: &environment,
        config_path: &config_path,
        env_file: format!("/etc/{name}/{environment}.env"),
    };
    let mut files = Vec::new();
    let mut alerting = false;

    if schedule.is_some() {
        let (service, timer) = units.scheduled(config)?;
        alerting |= service.contents.contains("OnFailure=");
        files.push(service);
        files.push(timer);
    }
    for entry in retain {
        let (service, timer) = units.retain(entry)?;
        files.push(service);
        files.push(timer);
        alerting = true;
    }
    if let Some(spec) = drill_schedule {
        let on_calendar = translate(spec, "drill", "schedule")?;
        files.push(
            units.unit(
                "drill.service",
                &units
                    .fill(DRILL_SERVICE)
                    .replace("{failure}", FAILURE_RECORD),
            ),
        );
        files.push(
            units.unit(
                "drill.timer",
                &units
                    .fill(DRILL_TIMER)
                    .replace("{on_calendar}", &on_calendar),
            ),
        );
        alerting = true;
    }
    if alerting {
        files.push(units.unit("alert@.service", &units.fill(ALERT_SERVICE)));
    }
    Ok(files)
}

/// What every scheduled unit of one deployable is rendered from.
struct Units<'a> {
    name: &'a str,
    environment: &'a str,
    config_path: &'a str,
    env_file: String,
}

impl Units<'_> {
    /// `template` with the fields every unit shares filled in.
    fn fill(&self, template: &str) -> String {
        template
            .replace("{name}", self.name)
            .replace("{environment}", self.environment)
            .replace("{config_path}", self.config_path)
            .replace("{env_file}", &self.env_file)
    }

    /// The unit `fraisier-<name>-<environment>-<suffix>`, under the marker.
    fn unit(&self, suffix: &str, body: &str) -> GeneratedFile {
        let unit = format!("fraisier-{}-{}-{suffix}", self.name, self.environment);
        GeneratedFile {
            rel_path: PathBuf::from(format!("systemd/{unit}")),
            install_dest: Some(PathBuf::from(format!("/etc/systemd/system/{unit}"))),
            contents: with_header(body),
        }
    }

    /// The `[schedule]` service and timer. Only a backup alerts through a
    /// failure record: a deploy notifies in-process, so it must not also alert.
    fn scheduled(
        &self,
        config: &DeployConfig,
    ) -> Result<(GeneratedFile, GeneratedFile), ScaffoldError> {
        let schedule = config.schedule.as_ref();
        // `command` is explicit — there is no silent "deploy" default (an
        // unattended deploy must never be an accident); see the unattended-deploy
        // gate.
        let command = required(
            schedule.and_then(|s| s.command.as_deref()),
            "schedule",
            "command",
        )?;
        let on_calendar = resolve_on_calendar(schedule)?;
        let (notes, on_failure, failure) = if command == "backup" {
            let dir = config
                .backup
                .as_ref()
                .and_then(|backup| backup.dir.as_deref())
                .ok_or(ScaffoldError::MissingField {
                    section: "backup",
                    field: "dir",
                })?;
            (
                format!(
                    "# Each run writes a new timestamped archive into [backup].dir\n\
                     # ({}), so no run overwrites an earlier one.\n",
                    dir.display()
                ),
                self.fill(ON_FAILURE),
                FAILURE_RECORD,
            )
        } else {
            (String::new(), String::new(), "")
        };
        let flag = if failure.is_empty() {
            ""
        } else {
            " --failure-record %n"
        };
        let service = self
            .fill(SCHEDULED_SERVICE)
            .replace("{command}", &command)
            .replace("{notes}", &notes)
            .replace("{on_failure}", &on_failure)
            .replace("{flag}", flag)
            .replace("{failure}", failure);
        let timer = self
            .fill(SCHEDULED_TIMER)
            .replace("{command}", &command)
            .replace("{on_calendar}", &on_calendar);
        Ok((
            self.unit("scheduled.service", &service),
            self.unit("scheduled.timer", &timer),
        ))
    }

    /// The prune service and timer for one `[[backup.retain]]` entry.
    fn retain(&self, entry: &RetainEntry) -> Result<(GeneratedFile, GeneratedFile), ScaffoldError> {
        let retain = entry
            .name()
            .filter(|name| fraisier_config::is_unit_word(name))
            .ok_or_else(|| ScaffoldError::InvalidField {
                section: "backup.retain",
                field: "name",
                message: "a retain entry needs a name of letters, digits, '_', '.' or '-' \
                          (it names the entry's systemd units)"
                    .to_owned(),
            })?;
        let spec = required(entry.schedule.as_deref(), "backup.retain", "schedule")?;
        let on_calendar = translate(&spec, "backup.retain", "schedule")?;
        let service = self
            .fill(RETAIN_SERVICE)
            .replace("{retain}", &retain)
            .replace("{failure}", FAILURE_RECORD);
        let timer = self
            .fill(RETAIN_TIMER)
            .replace("{retain}", &retain)
            .replace("{on_calendar}", &on_calendar);
        Ok((
            self.unit(&format!("retain-{retain}.service"), &service),
            self.unit(&format!("retain-{retain}.timer"), &timer),
        ))
    }
}

/// A native calendar spec as an `OnCalendar=` expression.
fn translate(
    spec: &str,
    section: &'static str,
    field: &'static str,
) -> Result<String, ScaffoldError> {
    fraisier_config::calendar::to_on_calendar(spec).map_err(|message| ScaffoldError::InvalidField {
        section,
        field,
        message,
    })
}

/// A required string field, or [`ScaffoldError::MissingField`].
fn required(
    value: Option<&str>,
    section: &'static str,
    field: &'static str,
) -> Result<String, ScaffoldError> {
    value
        .filter(|s| !s.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or(ScaffoldError::MissingField { section, field })
}

/// Resolve the timer's `OnCalendar=` from the stable native `calendar` vocabulary
/// (translated) or the `on_calendar_raw` escape hatch — exactly one of which the
/// validator requires.
fn resolve_on_calendar(
    schedule: Option<&fraisier_config::ScheduleSection>,
) -> Result<String, ScaffoldError> {
    let schedule = schedule.ok_or(ScaffoldError::MissingField {
        section: "schedule",
        field: "calendar",
    })?;
    if let Some(spec) = schedule
        .calendar
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        return fraisier_config::calendar::to_on_calendar(spec).map_err(|message| {
            ScaffoldError::InvalidField {
                section: "schedule",
                field: "calendar",
                message,
            }
        });
    }
    if let Some(raw) = schedule
        .on_calendar_raw
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        return Ok(raw.to_owned());
    }
    Err(ScaffoldError::MissingField {
        section: "schedule",
        field: "calendar",
    })
}

/// The fields the templates are rendered from, resolved (with defaults) once.
struct Resolved {
    name: String,
    environment: String,
    unit: String,
    socket: String,
    exec_start: String,
    env_file: String,
    upstream: String,
    nginx_dest: PathBuf,
    db_env: String,
    server_block: String,
}

impl Resolved {
    fn from_config(config: &DeployConfig) -> Result<Self, ScaffoldError> {
        let deploy = config.deploy.as_ref();
        let name = deploy
            .and_then(|d| d.name.as_deref())
            .filter(|s| !s.trim().is_empty())
            .ok_or(ScaffoldError::MissingField {
                section: "deploy",
                field: "name",
            })?
            .to_owned();
        let environment = deploy
            .and_then(|d| d.environment.as_deref())
            .filter(|s| !s.trim().is_empty())
            .ok_or(ScaffoldError::MissingField {
                section: "deploy",
                field: "environment",
            })?
            .to_owned();

        let unit = config
            .service
            .as_ref()
            .and_then(|s| s.unit.as_deref())
            .map_or_else(|| format!("{name}.service"), ToOwned::to_owned);
        let socket = format!("{}.socket", unit.strip_suffix(".service").unwrap_or(&unit));
        let active_path = config
            .artifact
            .as_ref()
            .and_then(|a| a.active_path.as_deref())
            .map_or_else(
                || format!("/srv/{name}/current"),
                |p| p.display().to_string(),
            );
        let upstream = config
            .lb
            .as_ref()
            .and_then(|l| l.upstream.as_deref())
            .map_or_else(|| format!("{name}_upstream"), ToOwned::to_owned);
        let nginx_dest = config
            .lb
            .as_ref()
            .and_then(|l| l.config_path.clone())
            .unwrap_or_else(|| PathBuf::from(format!("/etc/nginx/sites-available/{name}")));
        let db_env = config
            .migration
            .as_ref()
            .and_then(|m| m.database_url_env.as_deref())
            .map_or_else(
                || format!("{}_DATABASE_URL", name.to_uppercase()),
                ToOwned::to_owned,
            );

        Ok(Self {
            exec_start: format!("{active_path}/{name}"),
            env_file: format!("/etc/{name}/{environment}.env"),
            server_block: host_addresses(config)
                .iter()
                .map(|address| format!("    server {address};"))
                .collect::<Vec<_>>()
                .join("\n"),
            name,
            environment,
            unit,
            socket,
            upstream,
            nginx_dest,
            db_env,
        })
    }
}

/// The upstream server addresses: one per inventory host (`address:8080`), or a
/// single localhost entry for a single-host config.
fn host_addresses(config: &DeployConfig) -> Vec<String> {
    match config.host_inventory() {
        Some(inventory) if !inventory.hosts().is_empty() => inventory
            .hosts()
            .iter()
            .map(|host| format!("{}:8080", host.address))
            .collect(),
        _ => vec!["127.0.0.1:8080".to_owned()],
    }
}

/// Prepend the fraisier-generated marker comment to a rendered body.
fn with_header(body: &str) -> String {
    format!("# {MARKER}\n{body}")
}

const SYSTEMD_SERVICE: &str = "[Unit]
Description={name} ({environment})
After=network.target

[Service]
Type=simple
# Edit ExecStart to your binary; {active_path} is the symlink fraisier swaps.
ExecStart={exec_start}
Restart=on-failure
EnvironmentFile=-{env_file}

[Install]
WantedBy=multi-user.target
";

const SYSTEMD_SOCKET: &str = "[Unit]
Description={name} socket ({environment})

[Socket]
ListenStream=8080
Accept=no

[Install]
WantedBy=sockets.target
";

const SCHEDULED_SERVICE: &str = "[Unit]
Description=fraisier scheduled {command} for {name} ({environment})
After=network-online.target
Wants=network-online.target
{on_failure}
[Service]
Type=oneshot
# Runs fraisier on the host; edit --config / add flags as needed.
{notes}ExecStart=fraisier {command} --config {config_path}{flag}
EnvironmentFile=-{env_file}
{failure}";

const SCHEDULED_TIMER: &str = "[Unit]
Description=fraisier scheduled {command} timer for {name} ({environment})

[Timer]
OnCalendar={on_calendar}
Persistent=true

[Install]
WantedBy=timers.target
";

/// The `[Unit]` line of every unit that alerts through a failure record.
const ON_FAILURE: &str = "OnFailure=fraisier-{name}-{environment}-alert@%n.service\n";

/// Where such a unit's failed run leaves its record (`--failure-record %n`).
const FAILURE_RECORD: &str = "# A failed run leaves its redacted reason here for the alert unit.
RuntimeDirectory=fraisier/failures
RuntimeDirectoryPreserve=yes
";

const RETAIN_SERVICE: &str = "[Unit]
Description=fraisier prune of {retain} archives for {name} ({environment})
OnFailure=fraisier-{name}-{environment}-alert@%n.service

[Service]
Type=oneshot
# The policy (dir, match, retention, floor) stays in the config: this
# unit only names the [[backup.retain]] entry.
ExecStart=fraisier backup prune --config {config_path} --name {retain} --apply --failure-record %n
EnvironmentFile=-{env_file}
{failure}";

const RETAIN_TIMER: &str = "[Unit]
Description=fraisier prune timer of {retain} archives for {name} ({environment})

[Timer]
# Exactly one OnCalendar=: systemd adds every non-empty assignment as
# another trigger rather than replacing the last.
OnCalendar={on_calendar}
# A host that was down at the hour prunes on boot.
Persistent=true
RandomizedDelaySec=300

[Install]
WantedBy=timers.target
";

const DRILL_SERVICE: &str = "[Unit]
Description=fraisier restore drill for {name} ({environment})
After=network-online.target
Wants=network-online.target
OnFailure=fraisier-{name}-{environment}-alert@%n.service

[Service]
Type=oneshot
ExecStart=fraisier backup drill --config {config_path} --failure-record %n
EnvironmentFile=-{env_file}
{failure}";

const DRILL_TIMER: &str = "[Unit]
Description=fraisier restore drill timer for {name} ({environment})

[Timer]
# Exactly one OnCalendar=: systemd adds every non-empty assignment as
# another trigger rather than replacing the last.
OnCalendar={on_calendar}
Persistent=true

[Install]
WantedBy=timers.target
";

const ALERT_SERVICE: &str = "[Unit]
Description=fraisier failure alert for {name} ({environment}): %i

[Service]
Type=oneshot
# Sends the failed unit's record through [notify].command, once. This
# unit has no OnFailure=: a failed alert is logged, never re-alerted.
ExecStart=fraisier notify --config {config_path} --unit %i
EnvironmentFile=-{env_file}
";

const NGINX_SITE: &str = "upstream {upstream} {
{server_block}
}

server {
    listen 80;
    server_name {name}.example.com;

    location / {
        proxy_pass http://{upstream};
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }
}
";

const CI_WORKFLOW: &str = "name: deploy {name}
on:
  push:
    branches: [main]
jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Trigger fraisier deploy webhook
        run: |
          curl -fsS -X POST \"$WEBHOOK_URL\" \\
            -H 'Content-Type: application/json' \\
            -d '{\"fraise\":\"{name}\",\"environment\":\"{environment}\"}'
        env:
          WEBHOOK_URL: ${{ secrets.FRAISIER_WEBHOOK_URL }}
";

const ENV_EXAMPLE: &str = "# Environment for {name} ({environment}).
# Copy to the deploy host as {env_file}.
{db_env}=postgres://user:password@localhost/{name}
";

#[cfg(test)]
mod tests {
    use super::{generate, generate_scheduled, MARKER};
    use fraisier_config::DeployConfig;

    const MULTI: &str = r#"
[deploy]
name = "checkout"
environment = "production"

[hosts]
strategy = "rolling"
inventory = [
  { name = "web-1", address = "10.0.0.1" },
  { name = "web-2", address = "10.0.0.2" },
]

[artifact]
source = "release"
release_url = "https://x/checkout-{version}.tar.gz"
checksum_url = "https://x/checkout-{version}.tar.gz.sha256"
active_path = "/srv/checkout/current"

[migration]
adapter = "confiture"
database_url_env = "CHECKOUT_DATABASE_URL"

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://{host.address}:8080/health"

[lb]
adapter = "nginx"
config_path = "/etc/nginx/sites-available/checkout"
upstream = "checkout_upstream"
"#;

    fn find<'a>(files: &'a [super::GeneratedFile], needle: &str) -> &'a super::GeneratedFile {
        files
            .iter()
            .find(|f| f.rel_path.to_string_lossy().contains(needle))
            .unwrap_or_else(|| panic!("no generated file matching {needle:?}"))
    }

    #[test]
    fn generate_produces_the_expected_file_set() {
        let cfg = DeployConfig::from_toml_str(MULTI).expect("parse");
        let files = generate(&cfg).expect("generate");
        // service, socket, nginx, CI workflow, .env.example
        assert_eq!(files.len(), 5, "five files: {files:?}");
        for needle in [
            "checkout.service",
            "checkout.socket",
            "nginx",
            "deploy.yml",
            ".env.example",
        ] {
            let _ = find(&files, needle);
        }
    }

    #[test]
    fn every_file_carries_the_marker() {
        let cfg = DeployConfig::from_toml_str(MULTI).expect("parse");
        for file in generate(&cfg).expect("generate") {
            assert!(
                file.contents.contains(MARKER),
                "{:?} must carry the marker",
                file.rel_path
            );
        }
    }

    #[test]
    fn systemd_unit_uses_the_configured_unit_name_and_exec_path() {
        let cfg = DeployConfig::from_toml_str(MULTI).expect("parse");
        let files = generate(&cfg).expect("generate");
        let service = find(&files, "checkout.service");
        assert!(
            service.contents.contains("/srv/checkout/current"),
            "{service:?}"
        );
        assert_eq!(
            service.install_dest.as_deref(),
            Some(std::path::Path::new("/etc/systemd/system/checkout.service"))
        );
    }

    #[test]
    fn nginx_upstream_lists_every_inventory_host() {
        let cfg = DeployConfig::from_toml_str(MULTI).expect("parse");
        let files = generate(&cfg).expect("generate");
        let nginx = find(&files, "nginx");
        assert!(
            nginx.contents.contains("upstream checkout_upstream"),
            "{nginx:?}"
        );
        assert!(nginx.contents.contains("10.0.0.1"), "host 1: {nginx:?}");
        assert!(nginx.contents.contains("10.0.0.2"), "host 2: {nginx:?}");
        assert_eq!(
            nginx.install_dest.as_deref(),
            Some(std::path::Path::new("/etc/nginx/sites-available/checkout"))
        );
    }

    #[test]
    fn repo_files_have_no_install_dest() {
        let cfg = DeployConfig::from_toml_str(MULTI).expect("parse");
        let files = generate(&cfg).expect("generate");
        assert!(find(&files, "deploy.yml").install_dest.is_none());
        assert!(find(&files, ".env.example").install_dest.is_none());
    }

    #[test]
    fn ci_workflow_keeps_github_actions_expression_syntax() {
        let cfg = DeployConfig::from_toml_str(MULTI).expect("parse");
        let files = generate(&cfg).expect("generate");
        let ci = find(&files, "deploy.yml");
        // The `${{ secrets.* }}` expressions must survive verbatim.
        assert!(ci.contents.contains("${{ secrets."), "{ci:?}");
    }

    #[test]
    fn generate_requires_name_and_environment() {
        let cfg = DeployConfig::from_toml_str("[service]\nadapter = \"systemd\"\n").expect("parse");
        assert!(generate(&cfg).is_err());
    }

    #[test]
    fn generate_scheduled_produces_a_timer_and_oneshot_service() {
        // Native `calendar` is translated to OnCalendar=; `command` is explicit.
        let toml = format!(
            "{MULTI}{BACKUP_DIR}[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"backup\"\n"
        );
        let cfg = DeployConfig::from_toml_str(&toml).expect("parse");
        let files = generate_scheduled(&cfg).expect("generate");
        assert_eq!(files.len(), 3, "timer + service + alert: {files:?}");

        let timer = find(&files, "scheduled.timer");
        assert!(
            timer.contents.contains("OnCalendar=*-*-* 03:00:00"),
            "native calendar translated: {timer:?}"
        );

        let service = find(&files, "scheduled.service");
        assert!(
            service
                .contents
                .contains("ExecStart=fraisier backup --config"),
            "{service:?}"
        );
        assert!(service.contents.contains(MARKER), "marker: {service:?}");
        assert_eq!(
            service.install_dest.as_deref(),
            Some(std::path::Path::new(
                "/etc/systemd/system/fraisier-checkout-production-scheduled.service"
            ))
        );

        let timer = find(&files, "scheduled.timer");
        assert!(
            timer.contents.contains("OnCalendar=*-*-* 03:00:00"),
            "{timer:?}"
        );
        assert!(
            timer.contents.contains("WantedBy=timers.target"),
            "{timer:?}"
        );
    }

    /// The `[backup]` section a scheduled backup needs.
    const BACKUP_DIR: &str = "\n[backup]\ndir = \"/var/backups/checkout\"\n\n";

    #[test]
    fn a_scheduled_backup_service_is_exactly_this_unit() {
        let toml = format!(
            "{MULTI}{BACKUP_DIR}[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"backup\"\n"
        );
        let cfg = DeployConfig::from_toml_str(&toml).expect("parse");
        let files = generate_scheduled(&cfg).expect("generate");
        let service = find(&files, "scheduled.service");
        assert_eq!(
            service.contents,
            format!(
                "# {MARKER}\n\
                 [Unit]\n\
                 Description=fraisier scheduled backup for checkout (production)\n\
                 After=network-online.target\n\
                 Wants=network-online.target\n\
                 OnFailure=fraisier-checkout-production-alert@%n.service\n\
                 \n\
                 [Service]\n\
                 Type=oneshot\n\
                 # Runs fraisier on the host; edit --config / add flags as needed.\n\
                 # Each run writes a new timestamped archive into [backup].dir\n\
                 # (/var/backups/checkout), so no run overwrites an earlier one.\n\
                 ExecStart=fraisier backup --config /etc/fraisier/checkout.toml \
                 --failure-record %n\n\
                 EnvironmentFile=-/etc/checkout/production.env\n\
                 # A failed run leaves its redacted reason here for the alert unit.\n\
                 RuntimeDirectory=fraisier/failures\n\
                 RuntimeDirectoryPreserve=yes\n"
            )
        );
    }

    #[test]
    fn a_scheduled_backup_without_a_backup_dir_is_refused() {
        let toml =
            format!("{MULTI}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"backup\"\n");
        let cfg = DeployConfig::from_toml_str(&toml).expect("parse");
        let error = generate_scheduled(&cfg).expect_err("no [backup].dir");
        assert!(error.to_string().contains("backup"), "{error}");
        assert!(error.to_string().contains("dir"), "{error}");
    }

    #[test]
    fn generate_scheduled_requires_a_calendar_and_command() {
        // MULTI has no [schedule] section.
        let cfg = DeployConfig::from_toml_str(MULTI).expect("parse");
        assert!(generate_scheduled(&cfg).is_err());
    }

    #[test]
    fn generate_scheduled_accepts_the_raw_escape_hatch() {
        let toml = format!(
            "{MULTI}{BACKUP_DIR}[schedule]\non_calendar_raw = \"Mon *-*-* 04:00:00\"\ncommand = \"backup\"\n"
        );
        let cfg = DeployConfig::from_toml_str(&toml).expect("parse");
        let files = generate_scheduled(&cfg).expect("generate");
        let timer = find(&files, "scheduled.timer");
        assert!(
            timer.contents.contains("OnCalendar=Mon *-*-* 04:00:00"),
            "raw passthrough: {timer:?}"
        );
    }
}

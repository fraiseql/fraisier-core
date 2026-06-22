//! Integration tests for `fraisier-config`: parsing the PRD §7.1 example, the
//! §7.1a SpecQL preset expansion, the Decision-5 secret mapping, and the
//! separate validation pass.

use fraisier_config::{DeployConfig, Severity};
use fraisier_core::multi_host::RolloutStrategy;
use fraisier_core::single_host::PreflightMode;

/// The canonical PRD §7.1 `fraisier.toml` example.
const PRD_7_1: &str = r#"
[deploy]
name = "fraiseql"
environment = "production"

[hosts]
strategy = "rolling"
rolling_batch_size = 1
inventory = [
  { name = "web-1", address = "web1.internal" },
  { name = "web-2", address = "web2.internal" },
  { name = "web-3", address = "web3.internal" },
]

[artifact]
source = "release"
release_url = "https://github.com/example/fraiseql-{version}-musl.tar.gz"
checksum_url = "https://github.com/example/fraiseql-{version}-musl.tar.gz.sha256"

[migration]
adapter = "confiture"
database_url_env = "FRAISEQL_DATABASE_URL"
migrations_path = "./migrations"
forward_compatible_lint = true

[service]
adapter = "systemd"
unit = "fraiseql.service"

[health]
adapter = "http"
url = "http://{host.address}:8080/health"
expected_status = 200

[lb]
adapter = "nginx"
config_path = "/etc/nginx/sites-available/fraiseql"
upstream = "fraiseql_upstream"
"#;

#[test]
fn parses_the_prd_7_1_example() {
    let cfg = DeployConfig::from_toml_str(PRD_7_1).expect("parses");

    let deploy = cfg.deploy.as_ref().expect("deploy");
    assert_eq!(deploy.name.as_deref(), Some("fraiseql"));
    assert_eq!(deploy.environment.as_deref(), Some("production"));

    let migration = cfg.migration.as_ref().expect("migration");
    assert_eq!(migration.adapter.as_deref(), Some("confiture"));
    assert_eq!(migration.forward_compatible_lint, Some(true));

    let service = cfg.service.as_ref().expect("service");
    assert_eq!(service.unit.as_deref(), Some("fraiseql.service"));

    let lb = cfg.lb.as_ref().expect("lb");
    assert_eq!(lb.upstream.as_deref(), Some("fraiseql_upstream"));
}

#[test]
fn prd_example_validates_clean() {
    let cfg = DeployConfig::from_toml_str(PRD_7_1).expect("parses");
    let report = cfg.validate();
    assert!(report.ok(), "expected no errors, got: {report}");
}

#[test]
fn resolves_strategy_and_inventory_to_core_vocabulary() {
    let cfg = DeployConfig::from_toml_str(PRD_7_1).expect("parses");

    assert_eq!(cfg.rollout_strategy(), Some(RolloutStrategy::Rolling(1)));

    let inv = cfg.host_inventory().expect("inventory");
    assert_eq!(inv.hosts().len(), 3);
    assert_eq!(inv.hosts()[0].host.as_str(), "web-1");
    assert_eq!(inv.hosts()[0].address, "web1.internal");
}

#[test]
fn maps_database_url_env_to_logical_secret() {
    // Decision 5: database_url_env names the *source* env var; it maps onto the
    // logical name DATABASE_URL in env_secrets.
    let cfg = DeployConfig::from_toml_str(PRD_7_1).expect("parses");

    assert_eq!(
        cfg.migration_env_secrets()
            .get("DATABASE_URL")
            .map(String::as_str),
        Some("FRAISEQL_DATABASE_URL"),
    );

    let ctx = cfg.migration_adapter_ctx();
    assert_eq!(ctx.fraise, "fraiseql");
    assert_eq!(ctx.environment, "production");
    assert_eq!(
        ctx.env_secrets.get("DATABASE_URL").map(String::as_str),
        Some("FRAISEQL_DATABASE_URL"),
    );
    // The DSN *value* is never carried by the config or the context.
    assert!(!format!("{ctx:?}").contains("postgres://"));
}

#[test]
fn single_host_config_omits_hosts_and_lb() {
    let toml = r#"
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
    let cfg = DeployConfig::load(toml).expect("valid single-host config");
    assert!(cfg.hosts.is_none());
    assert!(cfg.lb.is_none());
    assert_eq!(cfg.rollout_strategy(), None);
    assert_eq!(cfg.host_inventory(), None);
    assert_eq!(cfg.health_expected_status(), 200);
}

#[test]
fn specql_preset_expands_to_a_full_config() {
    let toml = r#"
[specql]
name = "fraiseql"
schema = "./schema.toml"
environment = "production"
hosts = ["web1.internal", "web2.internal"]
"#;
    let cfg = DeployConfig::from_toml_str(toml).expect("parses");

    // The preset is consumed at load time; the resolved config has no [specql].
    assert!(cfg.specql.is_none());

    let deploy = cfg.deploy.as_ref().expect("deploy");
    assert_eq!(deploy.name.as_deref(), Some("fraiseql"));
    assert_eq!(deploy.environment.as_deref(), Some("production"));

    let migration = cfg.migration.as_ref().expect("migration");
    assert_eq!(migration.adapter.as_deref(), Some("confiture"));
    assert_eq!(migration.forward_compatible_lint, Some(true));

    let service = cfg.service.as_ref().expect("service");
    assert_eq!(service.adapter.as_deref(), Some("systemd"));
    assert_eq!(service.unit.as_deref(), Some("fraiseql.service"));

    let health = cfg.health.as_ref().expect("health");
    assert_eq!(health.adapter.as_deref(), Some("http"));

    // Two hosts → a multi-host rollout with an nginx LB.
    assert_eq!(cfg.rollout_strategy(), Some(RolloutStrategy::Rolling(1)));
    assert_eq!(cfg.host_inventory().expect("inventory").hosts().len(), 2);
    assert!(cfg.lb.is_some());

    assert!(cfg.validate().ok(), "preset output should validate clean");
}

/// `[migration].preflight_mode` parses each of the three values.
#[test]
fn preflight_mode_parses_each_value() {
    for (raw, expected) in [
        ("live", PreflightMode::Live),
        ("restore_rehearsal", PreflightMode::RestoreRehearsal),
        ("off", PreflightMode::Off),
    ] {
        let toml = format!(
            r#"
[deploy]
name = "x"
environment = "production"
[artifact]
source = "local"
path = "/srv/x"
[migration]
adapter = "confiture"
database_url_env = "X_DB"
preflight_mode = "{raw}"
[service]
adapter = "systemd"
unit = "x.service"
[health]
adapter = "http"
url = "http://127.0.0.1/health"
"#
        );
        let cfg = DeployConfig::from_toml_str(&toml).expect("parses");
        let migration = cfg.migration.as_ref().expect("migration");
        assert_eq!(migration.preflight_mode, Some(expected), "raw = {raw}");
        assert_eq!(
            migration.effective_preflight_mode(),
            expected,
            "raw = {raw}"
        );
    }
}

/// When `preflight_mode` is unset the effective mode defaults to `Live`, and the
/// legacy `forward_compatible_lint = false` flag still maps onto `Off` so existing
/// configs keep their opt-out semantics.
#[test]
fn preflight_mode_defaults_and_legacy_flag_mapping() {
    let base = |extra: &str| {
        format!(
            r#"
[deploy]
name = "x"
environment = "production"
[artifact]
source = "local"
path = "/srv/x"
[migration]
adapter = "confiture"
database_url_env = "X_DB"
{extra}
[service]
adapter = "systemd"
unit = "x.service"
[health]
adapter = "http"
url = "http://127.0.0.1/health"
"#
        )
    };

    // Nothing set → Live.
    let cfg = DeployConfig::from_toml_str(&base("")).expect("parses");
    let migration = cfg.migration.as_ref().expect("migration");
    assert_eq!(migration.preflight_mode, None);
    assert_eq!(migration.effective_preflight_mode(), PreflightMode::Live);

    // Legacy opt-out → Off.
    let cfg =
        DeployConfig::from_toml_str(&base("forward_compatible_lint = false")).expect("parses");
    let migration = cfg.migration.as_ref().expect("migration");
    assert_eq!(migration.effective_preflight_mode(), PreflightMode::Off);

    // Legacy opt-in → Live.
    let cfg = DeployConfig::from_toml_str(&base("forward_compatible_lint = true")).expect("parses");
    let migration = cfg.migration.as_ref().expect("migration");
    assert_eq!(migration.effective_preflight_mode(), PreflightMode::Live);

    // Explicit preflight_mode wins over the legacy flag.
    let cfg = DeployConfig::from_toml_str(&base(
        "forward_compatible_lint = false\npreflight_mode = \"restore_rehearsal\"",
    ))
    .expect("parses");
    let migration = cfg.migration.as_ref().expect("migration");
    assert_eq!(
        migration.effective_preflight_mode(),
        PreflightMode::RestoreRehearsal
    );
}

/// A health entry parses a `token_provider` and validates clean.
#[test]
fn health_token_provider_parses_and_validates() {
    let toml = r#"
[deploy]
name = "x"
environment = "production"
[artifact]
source = "local"
path = "/srv/x"
[migration]
adapter = "confiture"
database_url_env = "X_DB"
[service]
adapter = "systemd"
unit = "x.service"
[health]
adapter = "http"
url = "http://127.0.0.1/health"
[health.headers]
X-Trace-Id = "deploy"
[health.token_provider]
type = "oauth2_client_credentials"
token_url = "https://idp/token"
client_id = "svc"
client_secret_env = "IDP_SECRET"
scope = "api.read"
"#;
    let cfg = DeployConfig::from_toml_str(toml).expect("parses");
    assert!(cfg.validate().ok(), "{:?}", cfg.validate().issues);
    let provider = cfg
        .health
        .as_ref()
        .and_then(|h| h.token_provider.as_ref())
        .expect("token provider parsed");
    assert_eq!(provider.header(), "Authorization");
}

/// A bad `format`, a header collision, and a non-http adapter are each rejected.
#[test]
fn health_token_provider_validation_rejects_bad_configs() {
    let with = |health: &str| {
        format!(
            r#"
[deploy]
name = "x"
environment = "production"
[artifact]
source = "local"
path = "/srv/x"
[migration]
adapter = "confiture"
database_url_env = "X_DB"
[service]
adapter = "systemd"
unit = "x.service"
{health}
"#
        )
    };

    // Bad format (no {{token}}).
    let cfg = DeployConfig::from_toml_str(&with(
        "[health]\nadapter = \"http\"\nurl = \"http://h/health\"\n\
         [health.token_provider]\ntype = \"exec\"\ncommand = [\"t\"]\nformat = \"Bearer ABC\"\n",
    ))
    .expect("parses");
    assert!(!cfg.validate().ok(), "bad format must fail");

    // Header collision (case-insensitive) with a static header.
    let cfg = DeployConfig::from_toml_str(&with(
        "[health]\nadapter = \"http\"\nurl = \"http://h/health\"\n\
         [health.headers]\nauthorization = \"static\"\n\
         [health.token_provider]\ntype = \"exec\"\ncommand = [\"t\"]\n",
    ))
    .expect("parses");
    assert!(!cfg.validate().ok(), "header collision must fail");

    // token_provider on a non-http adapter.
    let cfg = DeployConfig::from_toml_str(&with(
        "[health]\nadapter = \"command\"\ncommand = \"scan\"\n\
         [health.token_provider]\ntype = \"exec\"\ncommand = [\"t\"]\n",
    ))
    .expect("parses");
    assert!(!cfg.validate().ok(), "token_provider needs http adapter");
}

/// `preflight_mode = "restore_rehearsal"` requires the confiture adapter, since
/// the rehearsal composes a generic-Postgres restore.
#[test]
fn restore_rehearsal_requires_the_confiture_adapter() {
    let toml = r#"
[deploy]
name = "x"
environment = "production"
[artifact]
source = "local"
path = "/srv/x"
[migration]
adapter = "command"
preflight_mode = "restore_rehearsal"
[migration.settings]
up = "true"
[service]
adapter = "systemd"
unit = "x.service"
[health]
adapter = "http"
url = "http://127.0.0.1/health"
"#;
    let cfg = DeployConfig::from_toml_str(toml).expect("parses");
    let report = cfg.validate();
    assert!(!report.ok(), "restore_rehearsal + non-confiture must error");
    assert!(
        report
            .issues
            .iter()
            .any(|i| i.path == "migration.preflight_mode" && i.severity == Severity::Error),
        "expected a preflight_mode error, got {:?}",
        report.issues
    );
}

#[test]
fn explicit_blocks_win_over_the_preset() {
    let toml = r#"
[specql]
name = "fraiseql"
schema = "./schema.toml"
environment = "production"
hosts = ["web1.internal"]

[service]
unit = "custom-fraiseql.service"
"#;
    let cfg = DeployConfig::from_toml_str(toml).expect("parses");
    let service = cfg.service.as_ref().expect("service");

    // The explicit `unit` overrides the preset's "<name>.service"...
    assert_eq!(service.unit.as_deref(), Some("custom-fraiseql.service"));
    // ...while the unset `adapter` still falls back to the preset default.
    assert_eq!(service.adapter.as_deref(), Some("systemd"));
}

#[test]
fn validation_locates_missing_database_url_env_for_confiture() {
    let toml = r#"
[deploy]
name = "checkout"
environment = "staging"

[artifact]
source = "release"
release_url = "https://example.com/app.tar.gz"
checksum_url = "https://example.com/app.tar.gz.sha256"

[migration]
adapter = "confiture"

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
"#;
    let cfg = DeployConfig::from_toml_str(toml).expect("parses");
    let report = cfg.validate();

    assert!(!report.ok());
    let issue = report
        .issues
        .iter()
        .find(|i| i.path == "migration.database_url_env")
        .expect("an issue located at migration.database_url_env");
    assert_eq!(issue.severity, Severity::Error);
    assert!(issue.message.contains("confiture"));
}

#[test]
fn validation_rejects_unknown_artifact_source() {
    let toml = r#"
[deploy]
name = "checkout"
environment = "staging"

[artifact]
source = "magic"

[migration]
adapter = "command"

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
"#;
    let cfg = DeployConfig::from_toml_str(toml).expect("parses");
    let report = cfg.validate();
    assert!(report
        .issues
        .iter()
        .any(|i| i.path == "artifact.source" && i.severity == Severity::Error));
}

#[test]
fn validation_rejects_empty_inventory() {
    let toml = r#"
[deploy]
name = "checkout"
environment = "staging"

[hosts]
strategy = "rolling"
inventory = []

[artifact]
source = "release"
release_url = "https://example.com/app.tar.gz"
checksum_url = "https://example.com/app.tar.gz.sha256"

[migration]
adapter = "command"

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
"#;
    let cfg = DeployConfig::from_toml_str(toml).expect("parses");
    let report = cfg.validate();
    assert!(report
        .issues
        .iter()
        .any(|i| i.path == "hosts.inventory" && i.severity == Severity::Error));
}

#[test]
fn load_surfaces_validation_errors() {
    let toml = r#"
[deploy]
environment = "staging"

[artifact]
source = "release"
release_url = "https://example.com/app.tar.gz"
checksum_url = "https://example.com/app.tar.gz.sha256"

[migration]
adapter = "command"

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
"#;
    // deploy.name is missing → load() must fail with the located error.
    let err = DeployConfig::load(toml).expect_err("missing deploy.name");
    let rendered = err.to_string();
    assert!(rendered.contains("deploy.name"), "got: {rendered}");
}

#[test]
fn unknown_top_level_key_is_a_located_parse_error() {
    let toml = r#"
[deploy]
name = "checkout"
environment = "staging"
nonsense = true
"#;
    let err = DeployConfig::from_toml_str(toml).expect_err("unknown field");
    // The toml parser's message names the offending key.
    assert!(err.to_string().contains("nonsense"), "got: {err}");
}

#[test]
fn release_without_active_path_warns_but_stays_valid() {
    // The PRD §7.1 example omits active_path: a warning, not an error.
    let cfg = DeployConfig::from_toml_str(PRD_7_1).expect("parses");
    let report = cfg.validate();
    assert!(report.ok(), "active_path is a warning, not an error");
    assert!(report
        .issues
        .iter()
        .any(|i| i.path == "artifact.active_path" && i.severity == Severity::Warning));
}

#[test]
fn rc_service_requires_a_name() {
    let base = |service: &str| {
        format!(
            r#"
[deploy]
name = "checkout"
environment = "staging"

[artifact]
source = "release"
release_url = "https://example.com/app.tar.gz"
checksum_url = "https://example.com/app.tar.gz.sha256"

[migration]
adapter = "command"

{service}

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
"#
        )
    };

    // rc without a name → a located error.
    let cfg = DeployConfig::from_toml_str(&base("[service]\nadapter = \"rc\"")).expect("parses");
    let report = cfg.validate();
    assert!(report
        .issues
        .iter()
        .any(|i| i.path == "service.name" && i.severity == Severity::Error));

    // rc with a name → clean.
    let cfg =
        DeployConfig::from_toml_str(&base("[service]\nadapter = \"rc\"\nname = \"fraiseql\""))
            .expect("parses");
    assert!(
        cfg.validate().ok(),
        "rc with a name should validate: {}",
        cfg.validate()
    );
}

#[test]
fn docker_compose_service_requires_a_compose_service() {
    let toml = |service: &str| {
        format!(
            r#"
[deploy]
name = "checkout"
environment = "staging"

[artifact]
source = "release"
release_url = "https://example.com/app.tar.gz"
checksum_url = "https://example.com/app.tar.gz.sha256"

[migration]
adapter = "command"

{service}

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
"#
        )
    };

    // docker-compose without compose_service → a located error.
    let cfg = DeployConfig::from_toml_str(&toml("[service]\nadapter = \"docker-compose\""))
        .expect("parses");
    assert!(cfg
        .validate()
        .issues
        .iter()
        .any(|i| i.path == "service.compose_service" && i.severity == Severity::Error));

    // docker-compose with compose_service (compose_file optional) → clean.
    let cfg = DeployConfig::from_toml_str(&toml(
        "[service]\nadapter = \"docker-compose\"\ncompose_service = \"web\"",
    ))
    .expect("parses");
    assert!(
        cfg.validate().ok(),
        "docker-compose with compose_service should validate: {}",
        cfg.validate()
    );
}

#[test]
fn active_path_and_staging_dir_parse() {
    let toml = r#"
[deploy]
name = "checkout"
environment = "staging"

[artifact]
source = "release"
release_url = "https://example.com/app.tar.gz"
checksum_url = "https://example.com/app.tar.gz.sha256"
active_path = "/var/lib/app/current"
staging_dir = "/var/lib/app/staging"

[migration]
adapter = "command"

[service]
adapter = "systemd"
unit = "x.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
"#;
    let cfg = DeployConfig::load(toml).expect("valid (active_path set → no warning)");
    let artifact = cfg.artifact.expect("artifact");
    assert_eq!(
        artifact.active_path.as_deref(),
        Some(std::path::Path::new("/var/lib/app/current"))
    );
    assert_eq!(
        artifact.staging_dir.as_deref(),
        Some(std::path::Path::new("/var/lib/app/staging"))
    );
}

const WITH_SERVICE_USER: &str = r#"
[deploy]
name = "app"
environment = "prod"

[artifact]
source = "release"
release_url = "https://example.com/app-{version}.tar.gz"
checksum_url = "https://example.com/app-{version}.tar.gz.sha256"

[migration]
adapter = "command"

[service]
adapter = "systemd"
unit = "app.service"
user = true

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
"#;

#[test]
fn service_user_parses_and_validates_for_systemd() {
    let cfg = DeployConfig::from_toml_str(WITH_SERVICE_USER).expect("parses");
    assert_eq!(cfg.service.as_ref().expect("service").user, Some(true));
    // user = true on systemd is valid (no error, no warning about it).
    let report = cfg.validate();
    assert!(report.ok(), "systemd + user is valid: {report}");
    assert!(
        !report.issues.iter().any(|i| i.path == "service.user"),
        "no service.user issue for systemd: {report}",
    );
}

#[test]
fn service_user_on_a_non_systemd_adapter_warns() {
    // The rc adapter ignores `user`; setting it should warn, not silently no-op.
    let toml = WITH_SERVICE_USER.replace(
        "adapter = \"systemd\"\nunit = \"app.service\"",
        "adapter = \"rc\"\nname = \"app\"",
    );
    let cfg = DeployConfig::from_toml_str(&toml).expect("parses");
    let report = cfg.validate();
    assert!(report.ok(), "a warning does not invalidate: {report}");
    let issue = report
        .issues
        .iter()
        .find(|i| i.path == "service.user")
        .expect("a service.user warning");
    assert_eq!(issue.severity, Severity::Warning);
}

/// A minimal valid base config the `[schedule]` tests append a section to.
const SCHEDULE_BASE: &str = r#"
[deploy]
name = "checkout"
environment = "production"

[artifact]
source = "local"
path = "/srv/checkout/build"
active_path = "/srv/checkout/current"

[migration]
adapter = "command"

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
"#;

fn has_error(report: &fraisier_config::ValidationReport, path: &str) -> bool {
    report
        .issues
        .iter()
        .any(|i| i.path == path && i.severity == Severity::Error)
}

#[test]
fn schedule_backup_with_native_calendar_validates_clean() {
    let toml =
        format!("{SCHEDULE_BASE}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"backup\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(report.ok(), "backup + native calendar is valid: {report}");
}

#[test]
fn schedule_requires_a_calendar_surface() {
    let toml = format!("{SCHEDULE_BASE}\n[schedule]\ncommand = \"backup\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "schedule.calendar"), "{report}");
}

#[test]
fn schedule_rejects_both_calendar_and_raw() {
    let toml = format!(
        "{SCHEDULE_BASE}\n[schedule]\ncalendar = \"hourly\"\non_calendar_raw = \"*-*-* 03:00:00\"\ncommand = \"backup\"\n"
    );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "schedule.calendar"), "{report}");
}

#[test]
fn schedule_rejects_a_malformed_native_calendar() {
    let toml =
        format!("{SCHEDULE_BASE}\n[schedule]\ncalendar = \"daily 25:00\"\ncommand = \"backup\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "schedule.calendar"), "{report}");
}

#[test]
fn schedule_command_is_explicit_no_default() {
    let toml = format!("{SCHEDULE_BASE}\n[schedule]\ncalendar = \"hourly\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "schedule.command"), "{report}");
}

#[test]
fn unattended_deploy_requires_the_optin_and_a_notify_sink() {
    // command = "deploy" without the opt-in or a sink: both are errors.
    let toml =
        format!("{SCHEDULE_BASE}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"deploy\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(
        has_error(&report, "schedule.allow_unattended_deploy"),
        "{report}"
    );
    assert!(has_error(&report, "schedule.notify"), "{report}");

    // With both, it validates clean.
    let ok = format!(
        "{SCHEDULE_BASE}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"deploy\"\n\
         allow_unattended_deploy = true\nnotify = \"systemd-cat -t fraisier\"\n"
    );
    let report = DeployConfig::from_toml_str(&ok).expect("parses").validate();
    assert!(report.ok(), "opted-in unattended deploy is valid: {report}");
}

const BLUE_GREEN: &str = r#"
[deploy]
name = "checkout"
environment = "production"
strategy = "blue-green"

[artifact]
source = "local"
path = "/srv/checkout/build"
active_path = "/srv/checkout/current"

[migration]
adapter = "command"

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"

[lb]
adapter = "nginx"
upstream = "checkout_upstream"
include_dir = "/etc/nginx/fraisier"

[blue_green]
green_unit = "checkout-green.service"
green_health_url = "http://127.0.0.1:8081/healthz"
green_servers = ["127.0.0.1:8081"]
blue_servers = ["127.0.0.1:8080"]
hold_secs = 30
green_pool = 20
connection_margin = 10
"#;

#[test]
fn blue_green_config_validates_clean() {
    let report = DeployConfig::from_toml_str(BLUE_GREEN)
        .expect("parses")
        .validate();
    assert!(report.ok(), "fully-specified blue-green is valid: {report}");
}

#[test]
fn blue_green_requires_its_section_and_lb() {
    // strategy = blue-green but no [blue_green] / [lb].include_dir.
    let toml = BLUE_GREEN
        .replace("include_dir = \"/etc/nginx/fraisier\"\n", "")
        .replace(
            "[blue_green]\ngreen_unit = \"checkout-green.service\"\n",
            "[blue_green]\n",
        );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "blue_green.green_unit"), "{report}");
    assert!(has_error(&report, "lb.include_dir"), "{report}");
}

#[test]
fn an_unknown_deploy_strategy_is_rejected() {
    let toml = BLUE_GREEN.replace("strategy = \"blue-green\"", "strategy = \"canary\"");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "deploy.strategy"), "{report}");
}

#[test]
fn the_old_on_calendar_key_no_longer_parses() {
    // The pre-GA breaking rename: `on_calendar` is gone (renamed on_calendar_raw).
    let toml = format!(
        "{SCHEDULE_BASE}\n[schedule]\non_calendar = \"*-*-* 03:00:00\"\ncommand = \"backup\"\n"
    );
    assert!(
        DeployConfig::from_toml_str(&toml).is_err(),
        "deny_unknown_fields must reject the removed `on_calendar` key"
    );
}

const TWO_CHECKS: &str = "
[[checks]]
name = \"lint\"
command = \"cargo clippy\"

[[checks]]
name = \"test\"
command = \"cargo test\"
";

#[test]
fn checks_parse_from_array_of_tables_and_validate_clean() {
    let toml = format!("{PRD_7_1}{TWO_CHECKS}");
    let cfg = DeployConfig::from_toml_str(&toml).expect("parses");
    assert_eq!(cfg.checks.len(), 2);
    assert_eq!(cfg.checks[0].name.as_deref(), Some("lint"));
    assert_eq!(cfg.checks[1].command.as_deref(), Some("cargo test"));
    let report = cfg.validate();
    assert!(report.ok(), "{report}");
}

#[test]
fn check_missing_name_is_an_error() {
    let toml = format!("{PRD_7_1}\n[[checks]]\ncommand = \"cargo test\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "checks[0].name"), "{report}");
}

#[test]
fn check_missing_command_is_an_error() {
    let toml = format!("{PRD_7_1}\n[[checks]]\nname = \"test\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "checks[0].command"), "{report}");
}

#[test]
fn duplicate_check_names_are_an_error() {
    let toml = format!(
        "{PRD_7_1}\n[[checks]]\nname = \"lint\"\ncommand = \"a\"\n\n\
         [[checks]]\nname = \"lint\"\ncommand = \"b\"\n"
    );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "checks"), "{report}");
}

#[test]
fn unknown_check_field_is_rejected() {
    let toml = format!("{PRD_7_1}\n[[checks]]\nname = \"x\"\ncommand = \"y\"\nbogus = 1\n");
    assert!(
        DeployConfig::from_toml_str(&toml).is_err(),
        "deny_unknown_fields must reject an unknown check key"
    );
}

#[test]
fn a_checks_only_config_validates_via_validate_checks_only() {
    // A project may carry only [[checks]] and no deploy sections.
    let toml = "[[checks]]\nname = \"lint\"\ncommand = \"cargo clippy\"\n";
    let cfg = DeployConfig::from_toml_str(toml).expect("parses");
    assert!(
        !cfg.validate().ok(),
        "a checks-only config is missing the deploy sections a full deploy needs"
    );
    let report = cfg.validate_checks_only();
    assert!(report.ok(), "{report}");
}

#[test]
fn an_absent_checks_list_is_not_an_error() {
    let cfg = DeployConfig::from_toml_str(PRD_7_1).expect("parses");
    assert!(cfg.checks.is_empty());
    assert!(cfg.validate_checks_only().ok());
}

#[test]
fn preset_overlay_keeps_author_checks() {
    // The [specql] preset emits no checks; an author-supplied [[checks]] must
    // survive preset expansion.
    let toml = format!(
        "[specql]\nname = \"app\"\nenvironment = \"production\"\nhosts = [\"h1.internal\"]\n{TWO_CHECKS}"
    );
    let cfg = DeployConfig::from_toml_str(&toml).expect("parses");
    assert_eq!(cfg.checks.len(), 2, "author checks must survive overlay");
    assert!(cfg.validate().ok(), "{}", cfg.validate());
}

/// A clean single-host config missing only its `[health]` section, so each
/// command-health test can append the health variant it exercises.
const HEALTH_BASE: &str = r#"
[deploy]
name = "checkout"
environment = "production"

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
"#;

#[test]
fn command_health_section_parses_and_round_trips() {
    let toml = format!(
        "{HEALTH_BASE}\n[health]\nadapter = \"command\"\n\
         command = \"fraiseql perf regression-scan --fail-on-regression\"\ntimeout_ms = 30000\n"
    );
    let cfg = DeployConfig::from_toml_str(&toml).expect("parses");
    let health = cfg.health.as_ref().expect("health");
    assert_eq!(health.adapter.as_deref(), Some("command"));
    assert_eq!(
        health.command.as_deref(),
        Some("fraiseql perf regression-scan --fail-on-regression")
    );
    assert_eq!(health.timeout_ms, Some(30_000));
    assert!(cfg.validate().ok(), "{}", cfg.validate());

    // serde round-trip: re-serialize and re-parse, the [health] section is stable.
    let serialized = toml::to_string(&cfg).expect("serialize");
    let reparsed = DeployConfig::from_toml_str(&serialized).expect("reparse");
    assert_eq!(reparsed.health, cfg.health);
}

#[test]
fn command_health_requires_a_command() {
    let toml = format!("{HEALTH_BASE}\n[health]\nadapter = \"command\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "health.command"), "{report}");
}

#[test]
fn command_health_rejects_url() {
    let toml = format!(
        "{HEALTH_BASE}\n[health]\nadapter = \"command\"\ncommand = \"scan\"\n\
         url = \"http://127.0.0.1:8080/health\"\n"
    );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "health.url"), "{report}");
}

#[test]
fn command_health_rejects_expected_status() {
    let toml = format!(
        "{HEALTH_BASE}\n[health]\nadapter = \"command\"\ncommand = \"scan\"\nexpected_status = 200\n"
    );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "health.expected_status"), "{report}");
}

#[test]
fn http_health_still_requires_url() {
    let toml = format!("{HEALTH_BASE}\n[health]\nadapter = \"http\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "health.url"), "{report}");
}

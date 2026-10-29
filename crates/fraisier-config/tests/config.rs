//! Integration tests for `fraisier-config`: parsing the PRD §7.1 example, the
//! §7.1a SpecQL preset expansion, the Decision-5 secret mapping, and the
//! separate validation pass.

use fraisier_config::{DeployConfig, Severity};
use fraisier_core::adapter_axes::RiskTier;
use fraisier_core::multi_host::RolloutStrategy;
use fraisier_core::policy::{PolicyAction, UnclassifiedAction};
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

/// A multi-host rollout has no restore rehearsal: it lints live instead. The
/// config must say so rather than let the rehearsal quietly not happen.
#[test]
fn restore_rehearsal_on_a_multi_host_config_warns_it_lints_live() {
    let toml = r#"
[deploy]
name = "x"
environment = "production"
[hosts]
strategy = "rolling"
inventory = [ { name = "web-1", address = "10.0.0.1" } ]
[ssh]
user = "deploy"
[artifact]
source = "local"
path = "/srv/x"
[migration]
adapter = "confiture"
database_url_env = "X_DATABASE_URL"
preflight_mode = "restore_rehearsal"
[service]
adapter = "systemd"
unit = "x.service"
[health]
adapter = "http"
url = "http://{host.address}/health"
"#;
    let cfg = DeployConfig::from_toml_str(toml).expect("parses");
    let report = cfg.validate();
    let issue = report
        .issues
        .iter()
        .find(|i| i.path == "migration.preflight_mode" && i.severity == Severity::Warning)
        .unwrap_or_else(|| panic!("expected a preflight_mode warning, got {report}"));
    assert!(issue.message.contains("[hosts]"), "{}", issue.message);
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
    let toml = format!(
        "{SCHEDULE_BASE}\n[backup]\ndir = \"/var/backups/checkout\"\n\
         [schedule]\ncalendar = \"daily 03:00\"\ncommand = \"backup\"\n"
    );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(report.ok(), "backup + native calendar is valid: {report}");
}

#[test]
fn a_scheduled_backup_requires_a_backup_dir() {
    // Without one, every run writes the same file in the unit's working
    // directory and fails from the second run on (#87).
    let toml =
        format!("{SCHEDULE_BASE}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"backup\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "backup.dir"), "{report}");
}

#[test]
fn the_backup_dir_must_be_absolute() {
    let toml = format!("{SCHEDULE_BASE}\n[backup]\ndir = \"backups\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "backup.dir"), "{report}");
}

#[test]
fn the_backup_dir_must_be_plain_with_no_parent_component() {
    // The scaffold writes backup.dir into a unit file, where a newline starts a
    // new directive; a retain.dir is held to the same rule.
    for dir in [
        "/var/backups/../etc",
        "/var/backups/a b",
        "/var/backups/x\\nExecStartPre=/bin/evil",
    ] {
        let toml = format!("{SCHEDULE_BASE}\n[backup]\ndir = \"{dir}\"\n");
        let report = DeployConfig::from_toml_str(&toml)
            .expect("parses")
            .validate();
        assert!(has_error(&report, "backup.dir"), "{dir:?}: {report}");
    }
}

#[test]
fn db_backup_validation_checks_the_backup_dir_it_writes_into() {
    let toml = format!("{SCHEDULE_BASE}\n[backup]\ndir = \"backups\"\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate_db_ops();
    assert!(has_error(&report, "backup.dir"), "{report}");
}

#[test]
fn a_backup_section_without_a_dir_is_an_error() {
    let toml = format!("{SCHEDULE_BASE}\n[backup]\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "backup.dir"), "{report}");
}

#[test]
fn an_absolute_backup_dir_validates_clean() {
    let toml = format!("{SCHEDULE_BASE}\n[backup]\ndir = \"/var/backups/checkout\"\n");
    let config = DeployConfig::from_toml_str(&toml).expect("parses");
    assert!(config.validate().ok(), "{}", config.validate());
    assert_eq!(
        config.backup.and_then(|backup| backup.dir),
        Some(std::path::PathBuf::from("/var/backups/checkout"))
    );
}

const BACKUP_DIR: &str = "[backup]\ndir = \"/var/backups/checkout\"\n\n";

const RETAIN: &str = "[[backup.retain]]\ndir = \"/var/backups/checkout\"\n\
                      schedule = \"daily 05:30\"\nretention_days = 3\n";

/// Validates `entry` under a `[backup]` section whose `dir` is RETAIN's.
fn retain_report(entry: &str) -> fraisier_config::ValidationReport {
    DeployConfig::from_toml_str(&format!("{SCHEDULE_BASE}\n{BACKUP_DIR}{entry}"))
        .expect("parses")
        .validate()
}

#[test]
fn a_retain_name_must_be_safe_in_a_unit_name() {
    // The name becomes part of a systemd unit name and an ExecStart= argument.
    for name in ["night ly", "-nightly", "nightly;rm", "100%", "a/b"] {
        let report = retain_report(&format!(
            "[[backup.retain]]\ndir = \"/var/backups/checkout\"\nname = \"{name}\"\n\
             schedule = \"daily 05:30\"\nretention_days = 14\n"
        ));
        assert!(
            has_error(&report, "backup.retain[0].name"),
            "{name:?}: {report}"
        );
    }
    // A name taken from `dir` is held to the same rule.
    let report = retain_report(
        "[[backup.retain]]\ndir = \"/var/backups/check out\"\n\
         schedule = \"daily 05:30\"\nretention_days = 14\n",
    );
    assert!(has_error(&report, "backup.retain[0].name"), "{report}");
}

#[test]
fn a_retain_entry_parses_with_its_defaults() {
    let config = DeployConfig::from_toml_str(&format!("{SCHEDULE_BASE}\n{BACKUP_DIR}{RETAIN}"))
        .expect("parses");
    assert!(config.validate().ok(), "{}", config.validate());
    let backup = config.backup.expect("[backup]");
    let [entry] = backup.retain.as_slice() else {
        panic!("one entry, got {:?}", backup.retain)
    };
    assert_eq!(
        entry.dir.as_deref(),
        Some(std::path::Path::new("/var/backups/checkout"))
    );
    assert_eq!(entry.schedule.as_deref(), Some("daily 05:30"));
    assert_eq!(entry.retention_days, Some(3));
    assert_eq!(entry.keep_minimum(), 3);
    assert_eq!(entry.keep_last, None);
    assert_eq!(entry.pattern(), "*.pgdump");
    assert_eq!(entry.name().as_deref(), Some("checkout"));
    assert_eq!(entry.min_free_gb, None);
}

#[test]
fn declared_retain_fields_win_over_the_defaults() {
    let config = DeployConfig::from_toml_str(&format!(
        "{SCHEDULE_BASE}\n{BACKUP_DIR}{RETAIN}keep_minimum = 5\nkeep_last = 9\n\
         match = \"checkout-production-*.pgdump\"\nname = \"prod-full\"\nmin_free_gb = 20\n"
    ))
    .expect("parses");
    assert!(config.validate().ok(), "{}", config.validate());
    let entry = &config.backup.expect("[backup]").retain[0];
    assert_eq!(entry.keep_minimum(), 5);
    assert_eq!(entry.keep_last, Some(9));
    assert_eq!(entry.pattern(), "checkout-production-*.pgdump");
    assert_eq!(entry.name().as_deref(), Some("prod-full"));
    assert_eq!(entry.min_free_gb, Some(20));
}

#[test]
fn a_retain_entry_requires_dir_schedule_and_retention_days() {
    for (field, line) in [
        ("dir", "dir = \"/var/backups/checkout\"\n"),
        ("schedule", "schedule = \"daily 05:30\"\n"),
        ("retention_days", "retention_days = 3\n"),
    ] {
        let report = retain_report(&RETAIN.replace(line, ""));
        assert!(
            has_error(&report, &format!("backup.retain[0].{field}")),
            "{field}: {report}"
        );
    }
}

#[test]
fn retention_days_must_be_at_least_one() {
    let report = retain_report(&RETAIN.replace("retention_days = 3", "retention_days = 0"));
    assert!(
        has_error(&report, "backup.retain[0].retention_days"),
        "{report}"
    );
}

#[test]
fn a_retain_dir_must_be_absolute_and_plain() {
    for bad in [
        "backups",
        "/backup/../etc",
        "/backup/$(whoami)",
        "/backup/a;rm -rf /",
        "/backup/%H",
        "/backup/prod\\nExecStartPost=/bin/rm",
    ] {
        let report = retain_report(&RETAIN.replace("/var/backups/checkout", bad));
        assert!(
            has_error(&report, "backup.retain[0].dir"),
            "{bad}: {report}"
        );
    }
}

#[test]
fn a_retain_match_is_confined_to_the_dir() {
    for bad in [
        "../staging/*.pgdump",
        "/etc/*.pgdump",
        "sub/*.pgdump",
        "*%i*.pgdump",
        "*\\n",
    ] {
        let report = retain_report(&format!("{RETAIN}match = \"{bad}\"\n"));
        assert!(
            has_error(&report, "backup.retain[0].match"),
            "{bad}: {report}"
        );
    }
}

#[test]
fn a_retain_match_selects_archives_only() {
    // The scheduled prune removes every old file the glob selects, so a glob
    // that can reach a file other than an archive can delete it.
    for bad in [
        "*",
        "*.sql",
        "checkout*",
        "*.pgdump*",
        "*.pgdump.partial",
        "*pgdump",
    ] {
        let report = retain_report(&format!("{RETAIN}match = \"{bad}\"\n"));
        assert!(
            has_error(&report, "backup.retain[0].match"),
            "{bad}: {report}"
        );
    }
    for good in [
        "*.pgdump",
        "checkout-production-*.pgdump",
        "checkout-?????.pgdump",
    ] {
        let report = retain_report(&format!("{RETAIN}match = \"{good}\"\n"));
        assert!(report.ok(), "{good}: {report}");
    }
}

#[test]
fn a_retain_dir_must_be_within_the_backup_dir() {
    for bad in [
        "/srv/app",
        "/var/backups",
        "/var/backups/checkout2",
        "/var/backups/checkout-old",
    ] {
        let report = retain_report(&RETAIN.replace("/var/backups/checkout", bad));
        assert!(
            has_error(&report, "backup.retain[0].dir"),
            "{bad}: {report}"
        );
    }
    for good in [
        "/var/backups/checkout/",
        "/var/backups/checkout/weekly",
        "/var/backups/./checkout",
    ] {
        let report = retain_report(&RETAIN.replace("/var/backups/checkout", good));
        assert!(report.ok(), "{good}: {report}");
    }
}

#[test]
fn a_retain_entry_requires_a_backup_dir() {
    let report = DeployConfig::from_toml_str(&format!("{SCHEDULE_BASE}\n{RETAIN}"))
        .expect("parses")
        .validate();
    assert!(has_error(&report, "backup.dir"), "{report}");
}

#[test]
fn a_retain_schedule_uses_the_calendar_vocabulary() {
    let report = retain_report(&RETAIN.replace("daily 05:30", "every tuesday"));
    assert!(has_error(&report, "backup.retain[0].schedule"), "{report}");
}

#[test]
fn a_retain_name_must_be_a_safe_identifier() {
    for bad in ["prod duction", "prod/uction", "prod%n", "prod$X", ""] {
        let report = retain_report(&format!("{RETAIN}name = \"{bad}\"\n"));
        assert!(
            has_error(&report, "backup.retain[0].name"),
            "{bad}: {report}"
        );
    }
}

#[test]
fn a_derived_name_must_also_be_safe() {
    let report = retain_report(&RETAIN.replace("/var/backups/checkout", "/"));
    assert!(has_error(&report, "backup.retain[0].name"), "{report}");
}

#[test]
fn retain_names_are_unique() {
    let report = retain_report(&format!("{RETAIN}{RETAIN}"));
    assert!(has_error(&report, "backup.retain[1].name"), "{report}");
}

#[test]
fn keep_last_below_keep_minimum_is_rejected() {
    let report = retain_report(&format!("{RETAIN}keep_minimum = 3\nkeep_last = 2\n"));
    assert!(has_error(&report, "backup.retain[0].keep_last"), "{report}");
}

#[test]
fn keep_last_of_zero_is_rejected() {
    let report = retain_report(&format!("{RETAIN}keep_minimum = 0\nkeep_last = 0\n"));
    assert!(has_error(&report, "backup.retain[0].keep_last"), "{report}");
}

#[test]
fn a_zero_floor_is_allowed() {
    let report = retain_report(&format!("{RETAIN}keep_minimum = 0\n"));
    assert!(report.ok(), "{report}");
}

#[test]
fn min_free_gb_must_be_at_least_one() {
    let report = retain_report(&format!("{RETAIN}min_free_gb = 0\n"));
    assert!(
        has_error(&report, "backup.retain[0].min_free_gb"),
        "{report}"
    );
}

#[test]
fn the_documented_retain_example_validates() {
    // An example that fails to load teaches the wrong shape, and nobody notices
    // because nothing runs it. Taken from the real page, not retyped.
    let page = include_str!("../../../docs/backups.md");
    let after = page
        .split_once("<!-- retain-example -->\n```toml\n")
        .expect("the marked example")
        .1;
    let example = after.split_once("```").expect("closing fence").0;
    let config = DeployConfig::from_toml_str(&format!("{SCHEDULE_BASE}\n{example}"))
        .expect("the example parses");
    assert!(config.validate().ok(), "{}", config.validate());
    let entry = &config.backup.expect("[backup]").retain[0];
    assert_eq!(entry.name().as_deref(), Some("nightly"));
    assert_eq!(entry.keep_last, Some(60));
}

#[test]
fn a_backup_section_with_neither_dir_nor_retain_is_still_an_error() {
    let report = DeployConfig::from_toml_str(&format!("{SCHEDULE_BASE}\n[backup]\nretain = []\n"))
        .expect("parses")
        .validate();
    assert!(has_error(&report, "backup.dir"), "{report}");
}

fn drill_report(section: &str) -> fraisier_config::ValidationReport {
    DeployConfig::from_toml_str(&format!(
        "{SCHEDULE_BASE}\n[backup]\ndir = \"/var/backups/checkout\"\n\n[drill]\n{section}"
    ))
    .expect("parses")
    .validate()
}

#[test]
fn an_empty_drill_section_takes_its_defaults() {
    let config = DeployConfig::from_toml_str(&format!(
        "{SCHEDULE_BASE}\n[backup]\ndir = \"/var/backups/checkout\"\n\n[drill]\n"
    ))
    .expect("parses");
    assert!(config.validate().ok(), "{}", config.validate());
    let drill = config.drill.expect("[drill]");
    assert_eq!(drill.max_age_hours(), 48);
    assert_eq!(drill.min_tables, None);
    assert_eq!(drill.data_dir, None);
}

#[test]
fn declared_drill_fields_are_kept() {
    let config = DeployConfig::from_toml_str(&format!(
        "{SCHEDULE_BASE}\n[backup]\ndir = \"/var/backups/checkout\"\n\n[drill]\n\
         max_age_hours = 26\nmin_tables = 15\ndata_dir = \"/var/lib/postgresql/18/main\"\n"
    ))
    .expect("parses");
    assert!(config.validate().ok(), "{}", config.validate());
    let drill = config.drill.expect("[drill]");
    assert_eq!(drill.max_age_hours(), 26);
    assert_eq!(drill.min_tables, Some(15));
    assert_eq!(
        drill.data_dir.as_deref(),
        Some(std::path::Path::new("/var/lib/postgresql/18/main"))
    );
}

#[test]
fn a_drill_needs_a_backup_dir_to_take_archives_from() {
    let report = DeployConfig::from_toml_str(&format!("{SCHEDULE_BASE}\n[drill]\n"))
        .expect("parses")
        .validate();
    assert!(has_error(&report, "drill"), "{report}");
}

#[test]
fn a_drill_schedule_is_kept_in_the_calendar_vocabulary() {
    let config = DeployConfig::from_toml_str(&format!(
        "{SCHEDULE_BASE}\n[backup]\ndir = \"/var/backups/checkout\"\n\n[drill]\n\
         schedule = \"weekly Sun 04:00\"\n"
    ))
    .expect("parses");
    assert!(config.validate().ok(), "{}", config.validate());
    assert_eq!(
        config.drill.and_then(|d| d.schedule).as_deref(),
        Some("weekly Sun 04:00")
    );
}

#[test]
fn a_malformed_drill_schedule_is_an_error() {
    assert!(has_error(
        &drill_report("schedule = \"every tuesday\"\n"),
        "drill.schedule"
    ));
}

#[test]
fn drill_max_age_hours_must_be_at_least_one() {
    assert!(has_error(
        &drill_report("max_age_hours = 0\n"),
        "drill.max_age_hours"
    ));
}

#[test]
fn drill_min_tables_must_be_at_least_one() {
    assert!(has_error(
        &drill_report("min_tables = 0\n"),
        "drill.min_tables"
    ));
}

#[test]
fn drill_data_dir_must_be_absolute() {
    assert!(has_error(
        &drill_report("data_dir = \"pgdata\"\n"),
        "drill.data_dir"
    ));
}

fn metrics_config(section: &str) -> DeployConfig {
    DeployConfig::from_toml_str(&format!("{SCHEDULE_BASE}\n[metrics]\n{section}")).expect("parses")
}

#[test]
fn a_metrics_textfile_dir_is_kept() {
    let config = metrics_config("textfile_dir = \"/var/lib/node_exporter/textfile\"\n");
    assert!(config.validate().ok(), "{}", config.validate());
    assert_eq!(
        config.metrics.expect("[metrics]").textfile_dir.as_deref(),
        Some(std::path::Path::new("/var/lib/node_exporter/textfile"))
    );
}

#[test]
fn a_metrics_section_needs_a_textfile_dir() {
    let config = metrics_config("");
    assert!(
        has_error(&config.validate(), "metrics.textfile_dir"),
        "{}",
        config.validate()
    );
}

#[test]
fn the_metrics_textfile_dir_must_be_absolute() {
    let config = metrics_config("textfile_dir = \"textfile\"\n");
    for report in [
        config.validate(),
        config.validate_db_ops(),
        config.validate_backup_ops(),
    ] {
        assert!(has_error(&report, "metrics.textfile_dir"), "{report}");
    }
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
    assert!(has_error(&report, "notify.command"), "{report}");

    // With both, it validates clean.
    let ok = format!(
        "{SCHEDULE_BASE}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"deploy\"\n\
         allow_unattended_deploy = true\n\n[notify]\ncommand = \"systemd-cat -t fraisier\"\n"
    );
    let report = DeployConfig::from_toml_str(&ok).expect("parses").validate();
    assert!(report.ok(), "opted-in unattended deploy is valid: {report}");
}

#[test]
fn schedule_notify_is_refused_with_a_pointer_to_notify_command() {
    // The sink moved: one spelling, so the old key is a targeted error, not an alias.
    let toml = format!(
        "{SCHEDULE_BASE}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"deploy\"\n\
         allow_unattended_deploy = true\nnotify = \"systemd-cat -t fraisier\"\n"
    );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    let issue = report
        .issues
        .iter()
        .find(|i| i.path == "schedule.notify" && i.severity == Severity::Error)
        .unwrap_or_else(|| panic!("{report}"));
    assert!(
        issue.message.contains("moved to `[notify].command`"),
        "{}",
        issue.message
    );
}

#[test]
fn a_notify_section_needs_a_command() {
    for body in ["", "command = \"  \"\n"] {
        let report = DeployConfig::from_toml_str(&format!("{SCHEDULE_BASE}\n[notify]\n{body}"))
            .expect("parses")
            .validate();
        assert!(has_error(&report, "notify.command"), "{body:?}: {report}");
    }
}

#[test]
fn a_notify_command_is_kept() {
    let config = DeployConfig::from_toml_str(&format!(
        "{SCHEDULE_BASE}\n[notify]\ncommand = \"systemd-cat -t fraisier\"\n"
    ))
    .expect("parses");
    assert!(config.validate().ok(), "{}", config.validate());
    assert_eq!(
        config.notify.and_then(|n| n.command).as_deref(),
        Some("systemd-cat -t fraisier")
    );
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
    assert_eq!(cfg.checks, Vec::new());
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

#[test]
fn http_health_require_capabilities_parses_and_validates() {
    let toml = format!(
        "{HEALTH_BASE}\n[health]\nadapter = \"http\"\nurl = \"http://127.0.0.1:8080/health\"\n\
         require_capabilities = [\"observers\", \"mcp\"]\n"
    );
    let cfg = DeployConfig::from_toml_str(&toml).expect("parses");
    assert!(cfg.validate().ok(), "{}", cfg.validate());
    assert_eq!(
        cfg.health.and_then(|h| h.require_capabilities),
        Some(vec!["observers".to_owned(), "mcp".to_owned()])
    );
}

#[test]
fn require_capabilities_rejects_an_empty_list() {
    let toml = format!(
        "{HEALTH_BASE}\n[health]\nadapter = \"http\"\nurl = \"http://127.0.0.1:8080/health\"\n\
         require_capabilities = []\n"
    );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(
        has_error(&report, "health.require_capabilities"),
        "{report}"
    );
}

#[test]
fn require_capabilities_rejects_duplicates() {
    let toml = format!(
        "{HEALTH_BASE}\n[health]\nadapter = \"http\"\nurl = \"http://127.0.0.1:8080/health\"\n\
         require_capabilities = [\"observers\", \"observers\"]\n"
    );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(
        has_error(&report, "health.require_capabilities"),
        "{report}"
    );
}

#[test]
fn require_capabilities_rejects_a_blank_name() {
    let toml = format!(
        "{HEALTH_BASE}\n[health]\nadapter = \"http\"\nurl = \"http://127.0.0.1:8080/health\"\n\
         require_capabilities = [\" \"]\n"
    );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(
        has_error(&report, "health.require_capabilities"),
        "{report}"
    );
}

#[test]
fn require_capabilities_applies_only_to_the_http_adapter() {
    let toml = format!(
        "{HEALTH_BASE}\n[health]\nadapter = \"command\"\ncommand = \"scan\"\n\
         require_capabilities = [\"observers\"]\n"
    );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(
        has_error(&report, "health.require_capabilities"),
        "{report}"
    );
}

// ---------------------------------------------------------------------------
// The [policy] section — the risk-keyed policy gate (issue #45)
// ---------------------------------------------------------------------------

/// A `[policy]` section stating every key.
const FULL_POLICY: &str = r#"
[policy]
auto_apply = ["additive"]
require_approval = ["lock_risky", "destructive", "irreversible"]
unclassified = "require_approval"
approval_command = "scripts/deploy/approve.sh"
approval_timeout_secs = 300
"#;

fn has_warning(report: &fraisier_config::ValidationReport, path: &str) -> bool {
    report
        .issues
        .iter()
        .any(|i| i.path == path && i.severity == Severity::Warning)
}

/// `PRD_7_1` plus the given `[policy]` body.
fn with_policy(body: &str) -> DeployConfig {
    DeployConfig::from_toml_str(&format!("{PRD_7_1}\n[policy]\n{body}")).expect("parses")
}

#[test]
fn a_policy_with_preflight_off_warns_that_every_deploy_will_be_refused() {
    // The combination is not a contradiction the operator can be assumed to
    // have meant: `preflight_mode = "off"` means nothing inspects the pending
    // schema changes, and a policy cannot approve what nobody has looked at, so
    // *every* deploy is refused at the gate. Correct, and baffling to debug from
    // the refusal alone — say it at config time, where the fix is.
    let off = PRD_7_1.replace("forward_compatible_lint = true", "preflight_mode = \"off\"");
    let report =
        DeployConfig::from_toml_str(&format!("{off}\n[policy]\nauto_apply = [\"additive\"]\n"))
            .expect("parses")
            .validate();
    assert!(has_warning(&report, "policy"), "{report}");
    // A warning, not an error: the operator may be mid-migration between the two
    // settings, and refusing to load the config would be worse than saying so.
    assert!(report.ok(), "{report}");
}

#[test]
fn a_policy_with_preflight_on_does_not_warn() {
    let report = with_policy("auto_apply = [\"additive\"]\n").validate();
    assert!(!has_warning(&report, "policy"), "{report}");
}

#[test]
fn an_unknown_policy_key_is_rejected() {
    let toml = format!("{PRD_7_1}\n[policy]\nauto_aply = [\"additive\"]\n");
    assert!(
        DeployConfig::from_toml_str(&toml).is_err(),
        "deny_unknown_fields must reject an unknown policy key"
    );
}

#[test]
fn an_unknown_tier_name_is_a_validation_error() {
    // A typo must not silently become an unlisted-and-therefore-denied tier:
    // that fails safe, and is baffling to debug.
    let report = with_policy("require_approval = [\"destructve\"]\n").validate();
    assert!(has_error(&report, "policy.require_approval"), "{report}");
    assert!(
        report
            .errors()
            .any(|i| i.message.contains("destructve") && i.message.contains("irreversible")),
        "the error must name the typo and the valid tiers: {report}"
    );
}

#[test]
fn a_tier_in_both_lists_is_a_validation_error() {
    // Ambiguous: refuse rather than pick.
    let report =
        with_policy("auto_apply = [\"lock_risky\"]\nrequire_approval = [\"lock_risky\"]\n")
            .validate();
    assert!(has_error(&report, "policy.auto_apply"), "{report}");
}

#[test]
fn unclassified_only_accepts_deny_or_require_approval() {
    let report = with_policy("unclassified = \"auto_apply\"\n").validate();
    assert!(has_error(&report, "policy.unclassified"), "{report}");
    // And the two legitimate values validate clean.
    for value in ["deny", "require_approval"] {
        let cfg = with_policy(&format!(
            "unclassified = \"{value}\"\napproval_command = \"a.sh\"\n"
        ));
        assert!(
            !has_error(&cfg.validate(), "policy.unclassified"),
            "{value} must be accepted"
        );
    }
}

#[test]
fn a_zero_approval_timeout_is_a_validation_error() {
    let report = with_policy("approval_timeout_secs = 0\napproval_command = \"a.sh\"\n").validate();
    assert!(
        has_error(&report, "policy.approval_timeout_secs"),
        "{report}"
    );
}

#[test]
fn an_approval_command_without_require_approval_tiers_warns() {
    // The hook would never run; say so rather than leaving it looking wired up.
    let report = with_policy(
        "auto_apply = [\"additive\"]\nrequire_approval = []\napproval_command = \"a.sh\"\n",
    )
    .validate();
    assert!(has_warning(&report, "policy.approval_command"), "{report}");
}

#[test]
fn require_approval_tiers_without_a_hook_warns() {
    // The other half: the policy asks for sign-off with nothing able to give
    // it, so every risky deploy will be refused at the gate. Fail-safe, but the
    // operator should learn it at config time, not mid-deploy.
    let report = with_policy("require_approval = [\"irreversible\"]\n").validate();
    assert!(has_warning(&report, "policy.approval_command"), "{report}");
}

#[test]
fn the_policy_section_parses_and_resolves() {
    let cfg = DeployConfig::from_toml_str(&format!("{PRD_7_1}{FULL_POLICY}")).expect("parses");
    let report = cfg.validate();
    assert!(report.ok(), "{report}");

    let policy = cfg.policy.as_ref().expect("policy").resolve();
    assert_eq!(policy.actions[&RiskTier::Additive], PolicyAction::AutoApply);
    assert_eq!(
        policy.actions[&RiskTier::Irreversible],
        PolicyAction::RequireApproval
    );
    // Listed in neither list, so it is denied by omission.
    assert!(!policy.actions.contains_key(&RiskTier::Reversible));
    assert_eq!(policy.unclassified, UnclassifiedAction::RequireApproval);
    assert!(policy.has_approval_hook);
}

#[test]
fn an_omitted_policy_key_takes_its_documented_default() {
    let cfg = with_policy("approval_command = \"a.sh\"\n");
    assert!(cfg.validate().ok(), "{}", cfg.validate());

    let policy = cfg.policy.as_ref().expect("policy").resolve();
    assert_eq!(
        policy,
        fraisier_core::policy::Policy::default().with_approval_hook(true)
    );
}

#[test]
fn no_policy_section_means_no_policy_at_all() {
    // Configuring the section is the opt-in. Absent stays absent — there is
    // no implicit default policy for a config that never asked for one.
    let cfg = DeployConfig::from_toml_str(PRD_7_1).expect("parses");
    assert!(cfg.policy.is_none());
    assert!(cfg.validate().ok(), "{}", cfg.validate());
}

/// A base whose `[migration]` names the DSN env var the readiness probe reads.
const REQUIRES_BASE: &str = r#"
[deploy]
name = "checkout"
environment = "production"

[artifact]
source = "local"
path = "/srv/checkout/build"
active_path = "/srv/checkout/current"

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

#[test]
fn database_requires_parses_and_validates_clean() {
    let toml =
        format!("{REQUIRES_BASE}\n[database.requires]\npostgres_major = 18\npg_tviews = true\n");
    let config = DeployConfig::from_toml_str(&toml).expect("parses");
    assert!(config.validate().ok(), "{}", config.validate());
    let requires = config
        .database
        .and_then(|database| database.requires)
        .expect("requires");
    assert_eq!(
        (requires.postgres_major, requires.pg_tviews),
        (Some(18), Some(true))
    );
}

#[test]
fn an_empty_database_requires_is_an_error() {
    let toml = format!("{REQUIRES_BASE}\n[database.requires]\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "database.requires"), "{report}");
}

#[test]
fn a_postgres_major_below_10_is_an_error() {
    // server_version_num's two-part numbering starts at 10; 9.6 is "906xx".
    let toml = format!("{REQUIRES_BASE}\n[database.requires]\npostgres_major = 9\n");
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(
        has_error(&report, "database.requires.postgres_major"),
        "{report}"
    );
}

#[test]
fn database_requires_needs_the_dsn_env_var() {
    let toml = format!(
        "{}\n[database.requires]\npg_tviews = true\n",
        REQUIRES_BASE
            .replace("database_url_env = \"CHECKOUT_DATABASE_URL\"\n", "")
            .replace("adapter = \"confiture\"", "adapter = \"command\"")
    );
    let report = DeployConfig::from_toml_str(&toml)
        .expect("parses")
        .validate();
    assert!(has_error(&report, "migration.database_url_env"), "{report}");
}

fn database_report(section: &str) -> fraisier_config::ValidationReport {
    DeployConfig::from_toml_str(&format!("{REQUIRES_BASE}\n{section}"))
        .expect("parses")
        .validate()
}

#[test]
fn an_admin_url_env_is_kept() {
    let config = DeployConfig::from_toml_str(&format!(
        "{REQUIRES_BASE}\n[database]\nadmin_url_env = \"CHECKOUT_ADMIN_URL\"\n"
    ))
    .expect("parses");
    assert!(config.validate().ok(), "{}", config.validate());
    assert_eq!(
        config.database.and_then(|d| d.admin_url_env).as_deref(),
        Some("CHECKOUT_ADMIN_URL")
    );
}

#[test]
fn a_blank_admin_url_env_is_an_error() {
    let report = database_report("[database]\nadmin_url_env = \" \"\n");
    assert!(has_error(&report, "database.admin_url_env"), "{report}");
}

#[test]
fn the_admin_identity_cannot_be_the_migration_dsn() {
    // A separate identity is the point: the same env var would make the app
    // role the superuser, or the superuser run migrations.
    let report = database_report("[database]\nadmin_url_env = \"CHECKOUT_DATABASE_URL\"\n");
    assert!(has_error(&report, "database.admin_url_env"), "{report}");
}

#[test]
fn on_empty_takes_fail_or_warn() {
    for value in ["fail", "warn"] {
        let report = database_report(&format!(
            "[database.requires]\npg_tviews = true\n\n[database.tviews]\non_empty = \"{value}\"\n"
        ));
        assert!(report.ok(), "{value}: {report}");
    }
    let config = DeployConfig::from_toml_str(&format!(
        "{REQUIRES_BASE}\n[database.requires]\npg_tviews = true\n\n[database.tviews]\non_empty = \"warn\"\n"
    ))
    .expect("parses");
    assert_eq!(
        config
            .database
            .and_then(|d| d.tviews)
            .and_then(|t| t.on_empty)
            .as_deref(),
        Some("warn")
    );
}

#[test]
fn on_empty_cannot_switch_the_gate_off() {
    for value in ["off", "ignore", ""] {
        let report = database_report(&format!(
            "[database.requires]\npg_tviews = true\n\n[database.tviews]\non_empty = \"{value}\"\n"
        ));
        assert!(
            has_error(&report, "database.tviews.on_empty"),
            "{value:?}: {report}"
        );
    }
}

#[test]
fn a_tviews_section_without_declaring_pg_tviews_is_an_error() {
    // The gate is declared by [database.requires].pg_tviews; a knob for a gate
    // that is not there would read as protection it does not give.
    let report = database_report("[database.tviews]\non_empty = \"warn\"\n");
    assert!(has_error(&report, "database.tviews"), "{report}");
}

#[test]
fn an_unknown_requirement_is_a_parse_error() {
    let toml = format!("{REQUIRES_BASE}\n[database.requires]\npostgis = true\n");
    assert!(DeployConfig::from_toml_str(&toml).is_err());
}

/// A subgraph names the endpoint a stack's SDL check asks; nothing else goes there.
#[test]
fn federation_names_the_graphql_endpoint() {
    let cfg = DeployConfig::from_toml_str(
        "[deploy]\nname = \"users\"\nenvironment = \"staging\"\n\
         [federation]\ngraphql_url = \"http://127.0.0.1:8101/graphql\"\n",
    )
    .expect("parses");
    assert_eq!(
        cfg.federation.and_then(|f| f.graphql_url).as_deref(),
        Some("http://127.0.0.1:8101/graphql")
    );
    assert!(DeployConfig::from_toml_str(
        "[federation]\ngraphql_url = \"http://u/graphql\"\nsdl_headers = {}\n"
    )
    .is_err());
}

fn database_less(extra: &str) -> String {
    format!(
        r#"
[deploy]
name = "gateway"
environment = "production"
[artifact]
source = "local"
path = "/srv/gateway"
[migration]
adapter = "none"
{extra}
[service]
adapter = "systemd"
unit = "gateway.service"
[health]
adapter = "http"
url = "http://127.0.0.1/readiness"
"#
    )
}

/// `adapter = "none"` declares a deployable that owns no database.
#[test]
fn a_database_less_migration_axis_validates() {
    let cfg = DeployConfig::from_toml_str(&database_less("")).expect("parses");
    let report = cfg.validate();
    assert!(report.ok(), "got {:?}", report.issues);
}

/// The router example in `docs/stacks.md` is the shape operators copy for a
/// stack's router member, so it must load and declare that it owns no database.
#[test]
fn the_documented_router_example_owns_no_database() {
    let page = include_str!("../../../docs/stacks.md");
    let after = page
        .split_once("<!-- router-example -->\n```toml\n")
        .expect("the marked example")
        .1;
    let example = after.split_once("```").expect("closing fence").0;
    let config = DeployConfig::from_toml_str(example).expect("the example parses");
    assert!(config.validate().ok(), "{}", config.validate());
    let migration = config.migration.expect("[migration]");
    assert_eq!(migration.adapter.as_deref(), Some("none"));
}

/// A database-less axis takes no DSN and no settings: either one means the
/// config believes there is a database after all.
#[test]
fn a_database_less_axis_refuses_a_dsn_or_settings() {
    for (extra, path) in [
        ("database_url_env = \"X_DB\"", "migration.database_url_env"),
        ("[migration.settings]\nup = \"true\"", "migration.settings"),
    ] {
        let cfg = DeployConfig::from_toml_str(&database_less(extra)).expect("parses");
        let report = cfg.validate();
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.path == path && i.severity == Severity::Error),
            "expected an error at {path}, got {:?}",
            report.issues
        );
    }
}

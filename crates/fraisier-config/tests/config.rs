//! Integration tests for `fraisier-config`: parsing the PRD §7.1 example, the
//! §7.1a SpecQL preset expansion, the Decision-5 secret mapping, and the
//! separate validation pass.

use fraisier_config::{DeployConfig, Severity};
use fraisier_core::multi_host::RolloutStrategy;

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

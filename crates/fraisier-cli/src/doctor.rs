//! `fraisier doctor` and `fraisier env-check`: host self-diagnosis and a
//! per-subcommand environment-variable preflight (Python fraisier 0.26).
//!
//! Both are read-only and side-effect-free. `doctor` runs a registry of
//! independent checks (one failing check never aborts the rest) and exits
//! `0`/`1`/`2` = pass/fail/warn. `env-check <subcommand>` reports which env vars
//! that subcommand would read and which are unset, exiting `0`/`1`/`2` =
//! all-set/some-unset/invalid-subcommand.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use fraisier_config::DeployConfig;
use serde_json::json;

use crate::commands::CommandOutput;

/// The confiture version floor the in-process adapter needs for both of its
/// version-gated capabilities — the window-safe verdict and the risk-tiered
/// change-set.
///
/// Tracks the highest floor in `fraisier-adapter-confiture`'s
/// `CAPABILITY_FLOORS`, and is pinned to it by test. Both sit on 0.44.0, for the
/// same underlying reason: through 0.43.0 confiture's classifier reads only part
/// of the statement matrix (fraiseql/confiture#206), so a `DROP TABLE` reports
/// `window_safe: true` — and a classifier that cannot be trusted about the
/// window cannot be trusted about the tier either. A floor below 0.44 would pass
/// exactly the versions a deploy denies.
const CONFITURE_FLOOR: (u32, u32) = (0, 44);

/// A single doctor check outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    const fn tag(self) -> &'static str {
        match self {
            Self::Pass => "ok",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
        }
    }
}

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
}

/// Load the config (if present) once; checks read from the parsed value.
fn load(config_path: &Path) -> Result<DeployConfig, String> {
    let toml =
        std::fs::read_to_string(config_path).map_err(|error| format!("cannot read: {error}"))?;
    DeployConfig::from_toml_str(&toml).map_err(|error| format!("parse error: {error}"))
}

/// Whether the env var named by `source` is set and non-empty in this process.
fn env_set(source: &str) -> bool {
    std::env::var(source).is_ok_and(|value| !value.is_empty())
}

/// Every (purpose, source-env-var) secret the config references.
pub(crate) fn referenced_secrets(config: &DeployConfig) -> Vec<(String, String)> {
    use fraisier_core::token_provider::TokenProvider;
    let mut out = Vec::new();
    if let Some(env) = config
        .migration
        .as_ref()
        .and_then(|m| m.database_url_env.clone())
    {
        out.push(("migration database DSN".to_owned(), env));
    }
    if let Some(env) = config.webhook.as_ref().and_then(|w| w.secret_env.clone()) {
        out.push(("webhook HMAC secret".to_owned(), env));
    }
    match config
        .health
        .as_ref()
        .and_then(|h| h.token_provider.as_ref())
    {
        Some(TokenProvider::Oauth2ClientCredentials(p)) => {
            out.push((
                "health token client_secret".to_owned(),
                p.client_secret_env.clone(),
            ));
        }
        Some(TokenProvider::Oauth2RefreshToken(p)) => {
            out.push((
                "health token refresh_token".to_owned(),
                p.refresh_token_env.clone(),
            ));
        }
        _ => {}
    }
    out
}

/// Run `confiture --version` and compare to the floor.
fn check_confiture(config: &DeployConfig) -> Check {
    let uses_confiture =
        config.migration.as_ref().and_then(|m| m.adapter.as_deref()) == Some("confiture");
    if !uses_confiture {
        return Check {
            name: "confiture",
            status: Status::Pass,
            detail: "not used by this config".to_owned(),
        };
    }
    let program =
        std::env::var("FRAISIER_CONFITURE_BIN").unwrap_or_else(|_| "confiture".to_owned());
    match Command::new(&program).arg("--version").output() {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            match parse_version(&text) {
                Some(version) if version >= CONFITURE_FLOOR => Check {
                    name: "confiture",
                    status: Status::Pass,
                    detail: format!(
                        "{}.{} (>= floor {}.{})",
                        version.0, version.1, CONFITURE_FLOOR.0, CONFITURE_FLOOR.1
                    ),
                },
                Some(version) => Check {
                    name: "confiture",
                    status: Status::Warn,
                    detail: format!(
                        "{}.{} is below the {}.{} floor; neither its window-safety verdict nor \
                         its risk classification is trusted (confiture#206), so a blue-green \
                         deploy is refused and a configured `[policy]` classifies nothing",
                        version.0, version.1, CONFITURE_FLOOR.0, CONFITURE_FLOOR.1
                    ),
                },
                None => Check {
                    name: "confiture",
                    status: Status::Warn,
                    detail: "could not parse `confiture --version`".to_owned(),
                },
            }
        }
        _ => Check {
            name: "confiture",
            status: Status::Warn,
            detail: format!("`{program}` not found on PATH (set FRAISIER_CONFITURE_BIN)"),
        },
    }
}

/// Parse a leading `<major>.<minor>` out of a `confiture --version` line.
fn parse_version(text: &str) -> Option<(u32, u32)> {
    let token = text.split_whitespace().find(|t| t.contains('.'))?;
    // Strip a leading `v`/name prefix, then keep the leading `<digits>.<digits>…`.
    let digits: String = token
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = digits.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Lists the throwaway databases on the server a DSN names.
type ListThrowaways<'a> = &'a dyn Fn(&str) -> Result<Vec<String>, String>;

/// List the throwaway databases a crashed drill or restore rehearsal left behind.
///
/// Only a config that creates throwaways (a `[drill]`, or `preflight_mode =
/// "restore_rehearsal"`) reaches the database for this. The check prints the
/// command that drops each one and never runs it: a throwaway may belong to a
/// drill or deploy that is still running.
fn check_throwaways(config: &DeployConfig, list: ListThrowaways<'_>) -> Option<Check> {
    use fraisier_core::single_host::PreflightMode;

    let migration = config.migration.as_ref();
    let rehearses =
        migration.is_some_and(|m| m.effective_preflight_mode() == PreflightMode::RestoreRehearsal);
    if config.drill.is_none() && !rehearses {
        return None;
    }
    let warn = |detail: String| {
        Some(Check {
            name: "leftover_throwaways",
            status: Status::Warn,
            detail,
        })
    };
    let Some(source) = migration.and_then(|m| m.database_url_env.as_deref()) else {
        return warn("cannot list them: [migration].database_url_env is not set".to_owned());
    };
    let Some(dsn) = std::env::var(source).ok().filter(|dsn| !dsn.is_empty()) else {
        return warn(format!("cannot list them: ${source} is not set"));
    };
    match list(&dsn) {
        Err(error) => warn(format!(
            "cannot list them: {}",
            fraisier_saga::redact::credentials(&error)
        )),
        Ok(names) if names.is_empty() => Some(Check {
            name: "leftover_throwaways",
            status: Status::Pass,
            detail: "none".to_owned(),
        }),
        Ok(names) => {
            let mut detail = format!(
                "{} throwaway database(s) left behind. Unless a drill or deploy is running, \
                 drop each with:",
                names.len()
            );
            for name in &names {
                let _ = write!(detail, "\n    DROP DATABASE \"{name}\" WITH (FORCE);");
            }
            warn(detail)
        }
    }
}

/// The databases carrying the throwaway marker on the server `dsn` names.
fn list_throwaways(dsn: &str) -> Result<Vec<String>, String> {
    let conn = fraisier_db::PgConn::parse(dsn).map_err(|e| format!("invalid database DSN: {e}"))?;
    // `_` is a LIKE wildcard, so the marker's underscores are escaped.
    let pattern = fraisier_core::drill::THROWAWAY_MARKER.replace('_', "\\_");
    let mut query = fraisier_db::psql_command(
        &conn,
        &format!("SELECT datname FROM pg_database WHERE datname LIKE '%{pattern}%' ORDER BY 1"),
    );
    query.arg("-tA");
    let output = query
        .output()
        .map_err(|e| format!("could not run psql: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Run the doctor checks against `config_path`, optionally filtered to `only`.
#[must_use]
pub(crate) fn doctor(config_path: &Path, only: &[String]) -> CommandOutput {
    let mut checks: Vec<Check> = Vec::new();

    match load(config_path) {
        Err(detail) => {
            checks.push(Check {
                name: "config_loads",
                status: Status::Fail,
                detail,
            });
            // Without a config the remaining checks cannot run; report just this.
        }
        Ok(config) => {
            checks.push(Check {
                name: "config_loads",
                status: Status::Pass,
                detail: config_path.display().to_string(),
            });
            let report = config.validate();
            checks.push(Check {
                name: "config_valid",
                status: if report.ok() {
                    if report.issues.is_empty() {
                        Status::Pass
                    } else {
                        Status::Warn
                    }
                } else {
                    Status::Fail
                },
                detail: format!("{} issue(s)", report.issues.len()),
            });
            for (purpose, source) in referenced_secrets(&config) {
                checks.push(Check {
                    name: "secret_readable",
                    status: if env_set(&source) {
                        Status::Pass
                    } else {
                        Status::Fail
                    },
                    detail: format!("{purpose}: ${source}"),
                });
            }
            checks.push(check_confiture(&config));
            checks.extend(check_throwaways(&config, &list_throwaways));
        }
    }

    if !only.is_empty() {
        checks.retain(|check| only.iter().any(|name| name == check.name));
    }

    let any_fail = checks.iter().any(|c| c.status == Status::Fail);
    let any_warn = checks.iter().any(|c| c.status == Status::Warn);
    let exit_code = if any_fail {
        1
    } else if any_warn {
        2
    } else {
        0
    };

    let pretty = checks
        .iter()
        .map(|c| format!("[{}] {}: {}", c.status.tag(), c.name, c.detail))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let json = json!({
        "exit_code": exit_code,
        "checks": checks
            .iter()
            .map(|c| json!({"name": c.name, "status": c.status.tag(), "detail": c.detail}))
            .collect::<Vec<_>>(),
    });
    CommandOutput {
        exit_code,
        pretty,
        json,
    }
}

/// The env vars a `subcommand` would read from this config, with set-state.
fn env_for_subcommand(config: &DeployConfig, subcommand: &str) -> Option<Vec<(String, bool)>> {
    let secrets = referenced_secrets(config);
    let pick = |purposes: &[&str]| -> Vec<(String, bool)> {
        secrets
            .iter()
            .filter(|(purpose, _)| purposes.iter().any(|p| purpose.starts_with(p)))
            .map(|(_, source)| (source.clone(), env_set(source)))
            .collect()
    };
    match subcommand {
        // Deploy-family commands touch the DB DSN and (for an authed probe) the
        // health token secret.
        "deploy" | "trigger-deploy" | "rollback" | "ship" => {
            Some(pick(&["migration database DSN", "health token"]))
        }
        "db" | "backup" => Some(pick(&["migration database DSN"])),
        "webhook-server" => Some(pick(&["webhook HMAC secret"])),
        // Read-only commands that touch no secrets.
        "list" | "status" | "validate-config" | "providers" | "doctor" | "init" => Some(Vec::new()),
        _ => None,
    }
}

/// `env-check <subcommand>`: which env vars the subcommand reads, and which unset.
#[must_use]
pub(crate) fn env_check(config_path: &Path, subcommand: &str) -> CommandOutput {
    let config = match load(config_path) {
        Ok(config) => config,
        Err(detail) => {
            return CommandOutput {
                exit_code: 2,
                pretty: format!("env-check: {detail}\n"),
                json: json!({ "ok": false, "error": detail }),
            };
        }
    };
    let Some(vars) = env_for_subcommand(&config, subcommand) else {
        let message = format!("env-check: unknown subcommand {subcommand:?}");
        return CommandOutput {
            exit_code: 2,
            pretty: format!("{message}\n"),
            json: json!({ "ok": false, "error": message }),
        };
    };
    let unset: Vec<&String> = vars
        .iter()
        .filter(|(_, set)| !set)
        .map(|(v, _)| v)
        .collect();
    let exit_code = i32::from(!unset.is_empty());
    let pretty = if vars.is_empty() {
        format!("{subcommand} reads no environment secrets\n")
    } else {
        vars.iter()
            .map(|(var, set)| format!("{} ${var}", if *set { "[set]  " } else { "[UNSET]" }))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    };
    let json = json!({
        "subcommand": subcommand,
        "vars": vars.iter().map(|(v, set)| json!({"var": v, "set": set})).collect::<Vec<_>>(),
        "unset": unset,
    });
    CommandOutput {
        exit_code,
        pretty,
        json,
    }
}

#[cfg(test)]
mod tests {
    use super::{check_throwaways, doctor, env_check, parse_version, Status, CONFITURE_FLOOR};
    use std::io::Write as _;

    fn write_config(body: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp");
        file.write_all(body.as_bytes()).expect("write");
        file
    }

    const VALID: &str = r#"
[deploy]
name = "app"
environment = "prod"
[artifact]
source = "local"
path = "/srv/app"
[migration]
adapter = "command"
[migration.settings]
up = "true"
[service]
adapter = "systemd"
unit = "app.service"
[health]
adapter = "http"
url = "http://127.0.0.1/health"
"#;

    fn throwaway_config(extra: &str) -> fraisier_config::DeployConfig {
        fraisier_config::DeployConfig::from_toml_str(&format!(
            "[deploy]\nname = \"app\"\nenvironment = \"prod\"\n\
             [migration]\nadapter = \"command\"\ndatabase_url_env = \"FRAISIER_DOCTOR_DSN\"\n\
             {extra}"
        ))
        .expect("config")
    }

    fn with_dsn<T>(dsn: Option<&str>, run: impl FnOnce() -> T) -> T {
        let _guard = crate::test_env::env_guard();
        match dsn {
            Some(dsn) => std::env::set_var("FRAISIER_DOCTOR_DSN", dsn),
            None => std::env::remove_var("FRAISIER_DOCTOR_DSN"),
        }
        let out = run();
        std::env::remove_var("FRAISIER_DOCTOR_DSN");
        out
    }

    const DRILL: &str = "[backup]\ndir = \"/var/backups/app\"\n[drill]\n";

    #[test]
    fn a_config_that_creates_no_throwaways_never_reaches_the_database() {
        let reached = std::cell::Cell::new(false);
        let check = with_dsn(Some("postgres://app@db/shop"), || {
            check_throwaways(&throwaway_config(""), &|_| {
                reached.set(true);
                Ok(vec![])
            })
        });
        assert!(check.is_none());
        assert!(!reached.get());
    }

    #[test]
    fn no_leftover_throwaway_passes() {
        let check = with_dsn(Some("postgres://app@db/shop"), || {
            check_throwaways(&throwaway_config(DRILL), &|_| Ok(vec![]))
        })
        .expect("a check");
        assert_eq!(check.status, Status::Pass, "{}", check.detail);
    }

    #[test]
    fn a_leftover_throwaway_warns_with_the_command_that_drops_it() {
        let name = "shop_fraisier_rh_41_1a2b";
        let check = with_dsn(Some("postgres://app:s3cret@db/shop"), || {
            check_throwaways(&throwaway_config(DRILL), &|_| Ok(vec![name.to_owned()]))
        })
        .expect("a check");
        assert_eq!(check.status, Status::Warn);
        assert!(
            check
                .detail
                .contains(&format!("DROP DATABASE \"{name}\" WITH (FORCE);")),
            "{}",
            check.detail
        );
        assert!(!check.detail.contains("s3cret"), "{}", check.detail);
    }

    #[test]
    fn a_restore_rehearsal_preflight_is_checked_too() {
        let config = fraisier_config::DeployConfig::from_toml_str(
            "[deploy]\nname = \"app\"\nenvironment = \"prod\"\n\
             [migration]\nadapter = \"command\"\ndatabase_url_env = \"FRAISIER_DOCTOR_DSN\"\n\
             preflight_mode = \"restore_rehearsal\"\n",
        )
        .expect("config");
        let check = with_dsn(Some("postgres://app@db/shop"), || {
            check_throwaways(&config, &|_| Ok(vec![]))
        });
        assert!(check.is_some());
    }

    #[test]
    fn an_unset_dsn_or_a_failed_listing_warns() {
        let unset = with_dsn(None, || {
            check_throwaways(&throwaway_config(DRILL), &|_| Ok(vec![]))
        })
        .expect("a check");
        assert_eq!(unset.status, Status::Warn);
        assert!(
            unset.detail.contains("FRAISIER_DOCTOR_DSN"),
            "{}",
            unset.detail
        );

        let failed = with_dsn(Some("postgres://app@db/shop"), || {
            check_throwaways(&throwaway_config(DRILL), &|_| {
                Err("connection refused".to_owned())
            })
        })
        .expect("a check");
        assert_eq!(failed.status, Status::Warn);
        assert!(
            failed.detail.contains("connection refused"),
            "{}",
            failed.detail
        );
    }

    #[test]
    fn parse_version_reads_major_minor() {
        assert_eq!(parse_version("confiture 0.31.0"), Some((0, 31)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2)));
        assert_eq!(parse_version("no version here"), None);
    }

    /// The floor `doctor` reports must be the floor the deploy actually
    /// enforces, or the check tells an operator they are ready for a deploy that
    /// will be refused.
    ///
    /// Read off the adapter's own constants rather than restated, so the two
    /// cannot drift apart silently. Both of its version-gated capabilities sit
    /// on 0.44.0 today; if one ever moves, `doctor` must follow the **higher**
    /// of them. Certifying a binary that a deploy refuses is the failure this
    /// check exists to prevent, and it is a failure in either direction: too low
    /// passes versions that will be denied, too high sends an operator chasing
    /// an upgrade nothing requires.
    #[test]
    fn the_doctor_floor_matches_the_versions_the_adapter_trusts() {
        let highest = fraisier_adapter_confiture::CAPABILITY_FLOORS
            .iter()
            .filter_map(|(_, floor)| *floor)
            .max()
            .expect("at least one capability has a pinned floor");
        assert_eq!(
            CONFITURE_FLOOR,
            (highest.0, highest.1),
            "doctor certifies {CONFITURE_FLOOR:?} but the adapter withholds a \
             capability below {highest:?}"
        );
        // Nothing is withheld from every version — a `None` floor would mean a
        // capability doctor cannot speak for at all, and this check would be
        // quietly ignoring it.
        assert!(
            fraisier_adapter_confiture::CAPABILITY_FLOORS
                .iter()
                .all(|(_, floor)| floor.is_some()),
            "a withheld capability is invisible to this check: {:?}",
            fraisier_adapter_confiture::CAPABILITY_FLOORS
        );
    }

    #[test]
    fn doctor_passes_on_a_valid_secret_free_config() {
        let file = write_config(VALID);
        let out = doctor(file.path(), &[]);
        // No hard failure (0 = clean, 2 = warnings only); no FAIL check.
        assert_ne!(out.exit_code, 1, "no check should fail: {}", out.pretty);
        assert!(!out.pretty.contains("[FAIL]"), "pretty: {}", out.pretty);
        assert!(out.pretty.contains("config_loads"));
        assert!(out.pretty.contains("config_valid"));
    }

    #[test]
    fn doctor_fails_when_a_referenced_secret_is_unset() {
        // A uniquely-named env var that is never set in the test environment.
        let cfg = VALID.replace(
            "adapter = \"command\"\n[migration.settings]\nup = \"true\"\n",
            "adapter = \"confiture\"\ndatabase_url_env = \"DOCTOR_TEST_NEVER_SET_DSN\"\n",
        );
        let file = write_config(&cfg);
        let out = doctor(file.path(), &[]);
        assert_eq!(
            out.exit_code, 1,
            "an unset referenced secret fails: {}",
            out.pretty
        );
        assert!(out.pretty.contains("DOCTOR_TEST_NEVER_SET_DSN"));
    }

    #[test]
    fn doctor_reports_a_parse_failure_as_a_single_fail() {
        let file = write_config("this is not toml = = =");
        let out = doctor(file.path(), &[]);
        assert_eq!(out.exit_code, 1);
        assert!(out.pretty.contains("config_loads"));
    }

    #[test]
    fn env_check_lists_unset_secrets_and_exits_1() {
        let cfg = VALID.replace(
            "adapter = \"command\"\n[migration.settings]\nup = \"true\"\n",
            "adapter = \"confiture\"\ndatabase_url_env = \"ENVCHECK_TEST_NEVER_SET\"\n",
        );
        let file = write_config(&cfg);
        let out = env_check(file.path(), "deploy");
        assert_eq!(out.exit_code, 1);
        assert!(out.pretty.contains("[UNSET]") && out.pretty.contains("ENVCHECK_TEST_NEVER_SET"));
    }

    #[test]
    fn env_check_rejects_an_unknown_subcommand_with_exit_2() {
        let file = write_config(VALID);
        let out = env_check(file.path(), "nonsense");
        assert_eq!(out.exit_code, 2);
    }

    #[test]
    fn env_check_reports_no_secrets_for_a_read_only_command() {
        let file = write_config(VALID);
        let out = env_check(file.path(), "list");
        assert_eq!(out.exit_code, 0);
        assert!(out.pretty.contains("no environment secrets"));
    }
}

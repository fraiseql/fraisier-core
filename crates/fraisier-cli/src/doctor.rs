//! `fraisier doctor` and `fraisier env-check`: host self-diagnosis and a
//! per-subcommand environment-variable preflight (Python fraisier 0.26).
//!
//! Both are read-only and side-effect-free. `doctor` runs a registry of
//! independent checks (one failing check never aborts the rest) and exits
//! `0`/`1`/`2` = pass/fail/warn. `env-check <subcommand>` reports which env vars
//! that subcommand would read and which are unset, exiting `0`/`1`/`2` =
//! all-set/some-unset/invalid-subcommand.

use std::path::Path;
use std::process::Command;

use fraisier_config::DeployConfig;
use serde_json::json;

use crate::commands::CommandOutput;

/// The confiture version floor the in-process adapter needs (window-safe verdict).
const CONFITURE_FLOOR: (u32, u32) = (0, 23);

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
                        "{}.{} is below the {}.{} floor (window-safe verdict needs the floor)",
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
    use super::{doctor, env_check, parse_version};
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

    #[test]
    fn parse_version_reads_major_minor() {
        assert_eq!(parse_version("confiture 0.31.0"), Some((0, 31)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2)));
        assert_eq!(parse_version("no version here"), None);
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

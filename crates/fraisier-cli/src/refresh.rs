//! `fraisier db mark-refresh-target`: mark this config's database, once, as the
//! one `fraisier db refresh` may replace.

use std::path::Path;

use anyhow::Result;
use fraisier_config::DeployConfig;
use fraisier_db::refresh::{marker_value, MarkerServer, MARKER_SETTING};
use serde_json::json;

use crate::commands::{self, CommandOutput};

mod flag;
mod run;
mod units;

pub(crate) use run::{db_refresh, db_refresh_clear_flag, Run};

/// An exit-1 refusal carrying `message`, credentials stripped.
fn refuse(message: &str) -> CommandOutput {
    let message = fraisier_saga::redact::credentials(message);
    CommandOutput {
        exit_code: 1,
        pretty: format!("{message}\n"),
        json: json!({ "ok": false, "error": message }),
    }
}

/// Why `config`'s database can never be a refresh target, read from the config
/// alone: it does not declare `[deploy] production = false`.
pub(crate) fn production_problem(config: &DeployConfig) -> Option<&'static str> {
    match config.deploy.as_ref().and_then(|deploy| deploy.production) {
        None => Some(
            "this config does not declare [deploy] production. A refresh target's config \
             must say production = false; an undeclared one is never taken to be \
             non-production",
        ),
        Some(true) => Some(
            "this config says [deploy] production = true, and a production database is \
             never a refresh target",
        ),
        Some(false) => None,
    }
}

/// Why `config` may not name a refresh target, before anything is read from
/// the database: it does not declare `[deploy] production = false`, has no
/// `[refresh]` section, or is invalid.
fn refuse_as_target(config: &DeployConfig, verb: &str) -> Option<CommandOutput> {
    if let Some(problem) = production_problem(config) {
        return Some(refuse(&format!(
            "refusing to {verb}: {problem}. Nothing was changed"
        )));
    }
    if config.refresh.is_none() {
        return Some(refuse(&format!(
            "refusing to {verb}: this config has no [refresh] section, so nothing would ever \
             refresh this database. Nothing was changed"
        )));
    }
    commands::refuse_invalid_db_config(config, verb)
}

/// The marker naming `config` as a refresh target.
pub(crate) fn expected_marker(config: &DeployConfig) -> String {
    let deploy = config.deploy.as_ref();
    marker_value(
        deploy.and_then(|d| d.name.as_deref()).unwrap_or_default(),
        deploy
            .and_then(|d| d.environment.as_deref())
            .unwrap_or_default(),
    )
}

/// `db mark-refresh-target`: mark this config's database as its refresh
/// target. A database already marked for another config is refused; clearing
/// that marker is a deliberate act, by hand.
pub(crate) async fn db_mark_refresh_target(config_path: &Path) -> Result<CommandOutput> {
    db_mark_refresh_target_with(config_path, &fraisier_db::restore::Postgres).await
}

/// [`db_mark_refresh_target`] with the server supplied by the caller.
pub(crate) async fn db_mark_refresh_target_with(
    config_path: &Path,
    server: &dyn MarkerServer,
) -> Result<CommandOutput> {
    let verb = "mark a refresh target";
    let config = commands::load(config_path)?;
    if let Some(refusal) = refuse_as_target(&config, verb) {
        return Ok(refusal);
    }
    let conn = match commands::resolve_pg_conn(&config) {
        Ok(conn) => conn,
        Err(error) => return Ok(commands::db_config_error(&error)),
    };
    let expected = expected_marker(&config);
    let database = conn.database();
    match server.marker(&conn).await {
        Err(error) => Ok(refuse(&format!(
            "could not read the refresh marker: {error}"
        ))),
        Ok(Some(found)) if found == expected => Ok(CommandOutput {
            exit_code: 0,
            pretty: format!("{database} is already the refresh target of {expected}\n"),
            json: json!({ "ok": true, "marker": expected, "changed": false }),
        }),
        Ok(Some(found)) => Ok(refuse(&format!(
            "{database} is already marked as the refresh target of '{found}'; refusing to \
             mark it for '{expected}'. If that is intended, clear it first: ALTER DATABASE \
             \"{database}\" RESET {MARKER_SETTING}"
        ))),
        Ok(None) => match server.marker_privilege(&conn).await {
            Err(error) => Ok(refuse(&format!(
                "could not check the marker privilege: {error}"
            ))),
            Ok(Some(problem)) => Ok(refuse(&format!("{problem}. Nothing was changed"))),
            Ok(None) => match server.set_marker(&conn, &expected).await {
                Ok(()) => Ok(CommandOutput {
                    exit_code: 0,
                    pretty: format!(
                        "marked {database} as the refresh target of {expected}: `fraisier db \
                         refresh` may now replace it\n"
                    ),
                    json: json!({ "ok": true, "marker": expected, "changed": true }),
                }),
                Err(error) => Ok(refuse(&format!(
                    "could not set the refresh marker: {error}"
                ))),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use fraisier_db::refresh::MarkerServer;
    use fraisier_db::PgConn;

    use super::db_mark_refresh_target_with;
    use crate::commands::CommandOutput;
    use crate::test_env::{block_on, env_guard};

    const DSN_ENV: &str = "FRAISIER_REFRESH_TEST_DSN";

    /// A marker server that answers from fixed values and records every call.
    #[derive(Default)]
    struct Fake {
        marker: Option<String>,
        privilege: Option<String>,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
    }

    #[async_trait]
    impl MarkerServer for Fake {
        async fn marker(&self, conn: &PgConn) -> Result<Option<String>, String> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("marker {}", conn.database()));
            Ok(self.marker.clone())
        }

        async fn marker_privilege(&self, _conn: &PgConn) -> Result<Option<String>, String> {
            self.calls.lock().expect("calls").push("privilege".into());
            Ok(self.privilege.clone())
        }

        async fn set_marker(&self, conn: &PgConn, value: &str) -> Result<(), String> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("set {} {value}", conn.database()));
            Ok(())
        }
    }

    const REFRESH: &str = "[refresh]\nsource_dir = \"/var/backups/checkout\"\n\
                           source_environment = \"production\"\n\
                           sanitize = \"/etc/checkout/sanitize.sql\"\n";

    fn config(production: Option<bool>, refresh: &str) -> tempfile::NamedTempFile {
        let production = production.map_or_else(String::new, |p| format!("production = {p}\n"));
        let mut file = tempfile::NamedTempFile::new().expect("tempfile");
        write!(
            file,
            "[deploy]\nname = \"checkout\"\nenvironment = \"staging\"\n{production}\
             [migration]\nadapter = \"command\"\ndatabase_url_env = \"{DSN_ENV}\"\n{refresh}"
        )
        .expect("write");
        file
    }

    fn mark(file: &tempfile::NamedTempFile, server: &Fake) -> CommandOutput {
        let _guard = env_guard();
        std::env::set_var(
            DSN_ENV,
            "postgres://app:s3cret@db.internal/checkout_staging",
        );
        let out = block_on(db_mark_refresh_target_with(file.path(), server)).expect("runs");
        std::env::remove_var(DSN_ENV);
        out
    }

    #[test]
    fn an_undeclared_production_key_is_refused_by_name_before_any_query() {
        let server = Fake::default();
        let out = mark(&config(None, REFRESH), &server);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("production = false"), "{}", out.pretty);
        assert!(out.pretty.contains("[deploy]"), "{}", out.pretty);
        assert!(server.calls().is_empty(), "{:?}", server.calls());
    }

    #[test]
    fn a_production_config_is_never_marked() {
        let server = Fake::default();
        let out = mark(&config(Some(true), ""), &server);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("production = true"), "{}", out.pretty);
        assert!(server.calls().is_empty(), "{:?}", server.calls());
    }

    #[test]
    fn a_config_without_refresh_is_not_a_refresh_target() {
        let server = Fake::default();
        let out = mark(&config(Some(false), ""), &server);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("[refresh]"), "{}", out.pretty);
        assert!(server.calls().is_empty(), "{:?}", server.calls());
    }

    #[test]
    fn an_unmarked_database_is_marked_for_this_config() {
        let server = Fake::default();
        let out = mark(&config(Some(false), REFRESH), &server);
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
        assert_eq!(
            server.calls(),
            vec![
                "marker checkout_staging",
                "privilege",
                "set checkout_staging checkout/staging"
            ]
        );
        assert_eq!(out.json["changed"], true);
    }

    #[test]
    fn marking_again_for_the_same_config_changes_nothing() {
        let server = Fake {
            marker: Some("checkout/staging".into()),
            ..Fake::default()
        };
        let out = mark(&config(Some(false), REFRESH), &server);
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
        assert_eq!(out.json["changed"], false);
        assert!(!server.calls().iter().any(|c| c.starts_with("set")));
    }

    #[test]
    fn a_database_marked_for_another_config_is_refused_with_the_reset() {
        let server = Fake {
            marker: Some("checkout/qa".into()),
            ..Fake::default()
        };
        let out = mark(&config(Some(false), REFRESH), &server);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("checkout/qa"), "{}", out.pretty);
        assert!(
            out.pretty
                .contains("ALTER DATABASE \"checkout_staging\" RESET fraisier.refresh_target"),
            "{}",
            out.pretty
        );
        assert!(!server.calls().iter().any(|c| c.starts_with("set")));
    }

    #[test]
    fn a_role_without_the_marker_grant_is_refused_with_it() {
        let server = Fake {
            privilege: Some("GRANT SET ON PARAMETER fraisier.refresh_target TO \"app\"".into()),
            ..Fake::default()
        };
        let out = mark(&config(Some(false), REFRESH), &server);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(
            out.pretty.contains("GRANT SET ON PARAMETER"),
            "{}",
            out.pretty
        );
        assert!(out.pretty.contains("Nothing was changed"), "{}", out.pretty);
        assert!(!server.calls().iter().any(|c| c.starts_with("set")));
    }

    #[test]
    fn no_output_carries_the_password() {
        let server = Fake {
            marker: Some("checkout/qa".into()),
            ..Fake::default()
        };
        let out = mark(&config(Some(false), REFRESH), &server);
        assert!(!out.pretty.contains("s3cret"), "{}", out.pretty);
        assert!(!out.json.to_string().contains("s3cret"), "{}", out.json);
    }
}

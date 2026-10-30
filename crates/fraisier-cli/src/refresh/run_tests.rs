use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use fraisier_db::refresh::{MarkerServer, RefreshServer, SchemaPrivilege};
use fraisier_db::restore::RestoreServer;
use fraisier_db::tviews::{Identity, Rebuilt};
use fraisier_db::PgConn;

use super::{db_refresh_with, Machinery, Units};
use crate::commands::CommandOutput;
use crate::test_env::{block_on, env_guard};

const DSN_ENV: &str = "FRAISIER_REFRESH_RUN_DSN";
const ADMIN_ENV: &str = "FRAISIER_REFRESH_RUN_ADMIN";
const LISTING: &str = "220; 1259 16400 TABLE public tb_item app_prod\n\
                       221; 0 16400 TABLE DATA public tb_item app_prod\n";

/// The clock: 2026-10-26 02:15 UTC.
fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_mins(29_883_075)
}

/// A server that answers from fixed values and records each call.
struct Server {
    marker: Option<&'static str>,
    connections: Vec<String>,
    restore_fails: bool,
    bytes: u64,
    tables: u64,
    calls: Mutex<Vec<String>>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            marker: Some("checkout/staging"),
            connections: Vec::new(),
            restore_fails: false,
            bytes: 0,
            tables: 3,
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl Server {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn record(&self, call: String) {
        self.calls.lock().expect("calls").push(call);
    }

    fn dropped(&self) -> bool {
        self.calls().iter().any(|call| call.starts_with("drop"))
    }
}

#[async_trait]
impl RestoreServer for Server {
    async fn list(&self, _archive: &Path) -> Result<String, String> {
        self.record("list".into());
        Ok(LISTING.to_owned())
    }

    async fn identity(&self, conn: &PgConn) -> Result<Identity, String> {
        Ok(Identity {
            user: conn.database().to_owned(),
            superuser: false,
            recovery: false,
            system_identifier: "7700958424225614334".to_owned(),
        })
    }

    async fn current_user(&self, _conn: &PgConn) -> Result<String, String> {
        Ok("app".to_owned())
    }

    async fn dump(&self, _conn: &PgConn, _out: &Path) -> Result<(), String> {
        unreachable!("a refresh never dumps")
    }

    async fn create(&self, _m: &PgConn, name: &str, _owner: Option<&str>) -> Result<(), String> {
        self.record(format!("create {name}"));
        Ok(())
    }

    async fn restore(
        &self,
        _conn: &PgConn,
        _archive: &Path,
        _clean: bool,
    ) -> Result<String, String> {
        unreachable!("a refresh restores from lists")
    }

    async fn drop(&self, _maintenance: &PgConn, name: &str) -> Result<(), String> {
        self.record(format!("drop {name}"));
        Ok(())
    }

    async fn rebuild(&self, _conn: &PgConn, _only_empty: bool) -> Result<Vec<Rebuilt>, String> {
        Ok(Vec::new())
    }

    async fn still_empty(&self, _conn: &PgConn) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl MarkerServer for Server {
    async fn marker(&self, _conn: &PgConn) -> Result<Option<String>, String> {
        self.record("marker".into());
        Ok(self.marker.map(str::to_owned))
    }

    async fn marker_privilege(&self, _conn: &PgConn) -> Result<Option<String>, String> {
        Ok(None)
    }

    async fn set_marker(&self, _conn: &PgConn, value: &str) -> Result<(), String> {
        self.record(format!("set_marker {value}"));
        Ok(())
    }
}

#[async_trait]
impl RefreshServer for Server {
    async fn connections(&self, _m: &PgConn, _db: &str) -> Result<Vec<String>, String> {
        self.record("connections".into());
        Ok(self.connections.clone())
    }

    async fn extensions_problem(
        &self,
        _conn: &PgConn,
        _extensions: &[String],
    ) -> Result<Option<String>, String> {
        Ok(None)
    }

    async fn database_bytes(&self, _conn: &PgConn) -> Result<u64, String> {
        self.record("database_bytes".into());
        Ok(self.bytes)
    }

    async fn table_count(&self, _conn: &PgConn) -> Result<u64, String> {
        self.record("table_count".into());
        Ok(self.tables)
    }

    async fn create_extensions(&self, _conn: &PgConn, _ext: &[String]) -> Result<(), String> {
        Ok(())
    }

    async fn schemas(&self, _conn: &PgConn) -> Result<Vec<String>, String> {
        Ok(vec!["public".to_owned()])
    }

    async fn schema_privilege(
        &self,
        _conn: &PgConn,
        _privilege: SchemaPrivilege,
        _schema: &str,
        _role: &str,
        _revoke: bool,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn restore_list(
        &self,
        _conn: &PgConn,
        _archive: &Path,
        _list: &str,
        _role: Option<&str>,
        _data_only: bool,
    ) -> Result<String, String> {
        self.record("restore_list".into());
        if self.restore_fails {
            Err("pg_restore: error: out of disk".into())
        } else {
            Ok(String::new())
        }
    }
}

/// Units that are all running until stopped, recording each call.
#[derive(Default)]
struct FakeUnits {
    stopped_already: Vec<&'static str>,
    calls: Mutex<Vec<String>>,
}

impl FakeUnits {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn of(&self, verb: &str) -> Vec<String> {
        self.calls()
            .iter()
            .filter_map(|call| call.strip_prefix(&format!("{verb} ")).map(str::to_owned))
            .collect()
    }
}

#[async_trait]
impl Units for FakeUnits {
    async fn is_active(&self, unit: &str) -> Result<bool, String> {
        Ok(!self.stopped_already.contains(&unit))
    }

    async fn stop(&self, unit: &str) -> Result<(), String> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("stop {unit}"));
        Ok(())
    }

    async fn start(&self, unit: &str) -> Result<(), String> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("start {unit}"));
        Ok(())
    }
}

const UNITS: &str = "units = [\"checkout-worker.service\", \"checkout-beat.timer\"]\n";

/// A refresh config in `dir`: archives in `<dir>/prod`, the migration's `up`
/// command `up`, `production` and `extra` lines in `[refresh]`.
fn config(dir: &Path, production: Option<bool>, up: &str, extra: &str) -> PathBuf {
    std::fs::create_dir_all(dir.join("prod")).expect("mkdir");
    let production = production.map_or_else(String::new, |p| format!("production = {p}\n"));
    let path = dir.join("fraisier.toml");
    std::fs::write(
        &path,
        format!(
            "[deploy]\nname = \"checkout\"\nenvironment = \"staging\"\n{production}\n\
             [service]\nadapter = \"systemd\"\nunit = \"checkout.service\"\n\n\
             [database]\nadmin_url_env = \"{ADMIN_ENV}\"\n\n\
             [migration]\nadapter = \"command\"\ndatabase_url_env = \"{DSN_ENV}\"\n\n\
             [migration.settings.commands]\nup = \"{up}\"\ncurrent_revision = \"true\"\n\n\
             [refresh]\nsource_dir = \"{}\"\nsource_environment = \"production\"\n\
             sanitize = \"/etc/checkout/sanitize.sql\"\n{extra}",
            dir.join("prod").display()
        ),
    )
    .expect("write config");
    path
}

/// An archive of checkout/production taken at `stamp`.
fn archive(dir: &Path, stamp: &str) -> PathBuf {
    let path = dir.join(format!("prod/checkout-production-{stamp}.pgdump"));
    std::fs::write(&path, b"PGDMP").expect("write archive");
    path
}

fn refresh(
    dir: &Path,
    config: &Path,
    execute: bool,
    server: &Server,
    units: &FakeUnits,
) -> CommandOutput {
    let _guard = env_guard();
    std::env::set_var(
        DSN_ENV,
        "postgres://app:s3cret@db.internal/checkout_staging",
    );
    std::env::set_var(ADMIN_ENV, "postgres://postgres:r00tpw@db.internal/postgres");
    let out = block_on(db_refresh_with(
        config,
        &dir.join("state"),
        execute,
        &Machinery {
            server,
            units,
            pg_restore: Path::new("true"),
            now: now(),
        },
    ))
    .expect("runs");
    std::env::remove_var(DSN_ENV);
    std::env::remove_var(ADMIN_ENV);
    out
}

#[test]
fn an_undeclared_production_key_is_refused_before_anything_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), None, "true", "");
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), FakeUnits::default());
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(out.pretty.contains("production = false"), "{}", out.pretty);
    assert!(server.calls().is_empty(), "{:?}", server.calls());
    assert!(units.calls().is_empty(), "{:?}", units.calls());
}

#[test]
fn a_config_whose_service_is_not_systemd_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = config(dir.path(), Some(false), "true", "");
    let toml = std::fs::read_to_string(&path).expect("read").replace(
        "adapter = \"systemd\"\nunit = \"checkout.service\"",
        "adapter = \"rc\"\nname = \"checkout\"",
    );
    std::fs::write(&path, toml).expect("write");
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), FakeUnits::default());
    let out = refresh(dir.path(), &path, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(out.pretty.contains("systemd"), "{}", out.pretty);
    assert!(units.calls().is_empty(), "{:?}", units.calls());
}

#[test]
fn no_archive_of_the_source_is_refused_before_any_unit_stops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", "");
    let (server, units) = (Server::default(), FakeUnits::default());
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(
        out.pretty.contains("no archive of checkout/production"),
        "{}",
        out.pretty
    );
    assert!(units.calls().is_empty(), "{:?}", units.calls());
}

#[test]
fn an_archive_older_than_max_age_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", "max_age_hours = 24\n");
    archive(dir.path(), "20261024T021500Z");
    let (server, units) = (Server::default(), FakeUnits::default());
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(
        out.pretty.contains("older than max_age_hours = 24"),
        "{}",
        out.pretty
    );
    assert!(units.calls().is_empty(), "{:?}", units.calls());
}

#[test]
fn a_data_dir_without_room_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let extra = format!("data_dir = \"{}\"\n", dir.path().display());
    let config = config(dir.path(), Some(false), "true", &extra);
    archive(dir.path(), "20261026T021500Z");
    let server = Server {
        bytes: u64::MAX / 2,
        ..Server::default()
    };
    let units = FakeUnits::default();
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(units.calls().is_empty(), "{:?}", units.calls());
    assert!(!server.dropped());
}

#[test]
fn without_yes_only_the_plan_is_shown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", UNITS);
    let newest = archive(dir.path(), "20261026T021500Z");
    archive(dir.path(), "20261025T021500Z");
    let (server, units) = (Server::default(), FakeUnits::default());
    let out = refresh(dir.path(), &config, false, &server, &units);
    assert_eq!(out.exit_code, 0, "{}", out.pretty);
    assert!(out.pretty.contains("--yes"), "{}", out.pretty);
    assert_eq!(out.json["archive"], newest.display().to_string());
    assert_eq!(
        out.json["units"],
        serde_json::json!([
            "checkout-beat.timer",
            "checkout-worker.service",
            "checkout.service"
        ])
    );
    assert!(units.calls().is_empty(), "{:?}", units.calls());
    assert!(!server.dropped());
    assert!(!out.pretty.contains("s3cret"), "{}", out.pretty);
}

#[test]
fn every_unit_is_stopped_timers_first_and_left_stopped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", UNITS);
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), FakeUnits::default());
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 0, "{}", out.pretty);
    assert_eq!(
        units.of("stop"),
        [
            "checkout-beat.timer",
            "checkout-worker.service",
            "checkout.service"
        ]
    );
    assert!(units.of("start").is_empty(), "{:?}", units.calls());
    assert!(server.dropped());
    assert!(
        server
            .calls()
            .contains(&"set_marker checkout/staging".to_owned()),
        "{:?}",
        server.calls()
    );
    assert_eq!(out.json["service"], "stopped");
    assert_eq!(out.json["complete"], false);
    assert!(out.pretty.contains("left stopped"), "{}", out.pretty);
    assert!(!out.pretty.contains("s3cret"), "{}", out.pretty);
    assert!(!out.json.to_string().contains("s3cret"), "{}", out.json);
}

#[test]
fn a_refusal_after_stopping_starts_the_running_units_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", UNITS);
    archive(dir.path(), "20261026T021500Z");
    let server = Server {
        connections: vec!["app (psql, pid 4242)".to_owned()],
        ..Server::default()
    };
    let units = FakeUnits {
        stopped_already: vec!["checkout-worker.service"],
        ..FakeUnits::default()
    };
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(
        out.pretty.contains("app (psql, pid 4242)"),
        "{}",
        out.pretty
    );
    assert!(!server.dropped());
    // Only what was running starts again, last stopped first.
    assert_eq!(
        units.of("start"),
        ["checkout.service", "checkout-beat.timer"]
    );
    assert_eq!(out.json["service"], "restarted");
}

#[test]
fn a_database_marked_for_another_config_is_never_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", "");
    archive(dir.path(), "20261026T021500Z");
    let server = Server {
        marker: Some("checkout/qa"),
        ..Server::default()
    };
    let units = FakeUnits::default();
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(out.pretty.contains("checkout/qa"), "{}", out.pretty);
    assert!(!server.dropped());
    assert_eq!(units.of("start"), ["checkout.service"]);
}

#[test]
fn a_failure_after_the_drop_leaves_the_units_stopped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", UNITS);
    archive(dir.path(), "20261026T021500Z");
    let server = Server {
        restore_fails: true,
        ..Server::default()
    };
    let units = FakeUnits::default();
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(server.dropped());
    assert!(units.of("start").is_empty(), "{:?}", units.calls());
    assert_eq!(out.json["service"], "stopped");
    assert!(out.pretty.contains("left stopped"), "{}", out.pretty);
}

#[test]
fn a_failed_migration_fails_the_refresh_and_leaves_the_units_stopped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "exit 3", "");
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), FakeUnits::default());
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert_eq!(out.json["failed_step"], "migrate");
    assert!(units.of("start").is_empty(), "{:?}", units.calls());
}

#[test]
fn fewer_tables_than_min_tables_fails_the_refresh() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", "min_tables = 10\n");
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), FakeUnits::default());
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert_eq!(out.json["failed_step"], "tables");
    assert!(out.pretty.contains("min_tables = 10"), "{}", out.pretty);
}

#[test]
fn the_migrations_run_with_the_app_dsn_and_never_the_admin_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let seen = dir.path().join("migration.env");
    let up = format!("env > {}", seen.display());
    let config = config(dir.path(), Some(false), &up, "");
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), FakeUnits::default());
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 0, "{}", out.pretty);
    let env = std::fs::read_to_string(&seen).expect("the migration ran");
    assert!(
        env.contains("app:s3cret@db.internal/checkout_staging"),
        "{env}"
    );
    assert!(!env.contains(ADMIN_ENV), "{env}");
    assert!(!env.contains("r00tpw"), "{env}");
}

#[test]
fn a_held_deploy_lock_is_refused_before_any_unit_stops() {
    use fraisier_saga::state_store::{FilesystemStateStore, FraiseKey, StateStore as _};

    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", "");
    archive(dir.path(), "20261026T021500Z");
    let store = FilesystemStateStore::new(dir.path().join("state")).expect("store");
    let guard = block_on(store.acquire_lock(&FraiseKey::new("checkout", "staging"))).expect("lock");
    let (server, units) = (Server::default(), FakeUnits::default());
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(out.pretty.contains("deploy lock"), "{}", out.pretty);
    assert!(units.calls().is_empty(), "{:?}", units.calls());
    assert!(!server.dropped());
    block_on(store.release_lock(guard)).expect("release");
}

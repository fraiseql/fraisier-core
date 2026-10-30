use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use fraisier_db::refresh::{MarkerServer, RefreshServer, SchemaPrivilege};
use fraisier_db::restore::RestoreServer;
use fraisier_db::tviews::{Identity, Rebuilt};
use fraisier_db::PgConn;

use super::{clear_flag_with, db_refresh_with, Machinery};
use crate::commands::CommandOutput;
use crate::refresh::units::Units;
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

/// Units that are all running until stopped, recording each call. Every
/// unit loads the flag file and starts with the flag set unless told
/// otherwise.
#[derive(Default)]
struct FakeUnits {
    stopped_already: Vec<&'static str>,
    without_flag_file: Vec<&'static str>,
    without_flag: Vec<&'static str>,
    no_main_pid: Vec<&'static str>,
    environ_unreadable: bool,
    flag: PathBuf,
    /// A file the sanitize step creates: each start records whether it exists.
    sanitized: Option<PathBuf>,
    calls: Mutex<Vec<String>>,
}

impl FakeUnits {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn record(&self, call: String) {
        self.calls.lock().expect("calls").push(call);
    }

    fn of(&self, verb: &str) -> Vec<String> {
        self.calls()
            .iter()
            .filter_map(|call| call.strip_prefix(&format!("{verb} ")).map(str::to_owned))
            .collect()
    }

    fn unit_of(pid: u32) -> &'static str {
        [
            "checkout.service",
            "checkout-worker.service",
            "checkout-beat.service",
        ][pid as usize - 100]
    }
}

#[async_trait]
impl Units for FakeUnits {
    async fn is_active(&self, unit: &str) -> Result<bool, String> {
        Ok(!self.stopped_already.contains(&unit))
    }

    async fn stop(&self, unit: &str) -> Result<(), String> {
        self.record(format!("stop {unit}"));
        Ok(())
    }

    async fn start(&self, unit: &str) -> Result<(), String> {
        let sanitized = self.sanitized.as_ref().map_or("", |path| {
            if path.exists() {
                " sanitized"
            } else {
                " unsanitized"
            }
        });
        let flagged = if self.flag.exists() { " flagged" } else { "" };
        self.record(format!("start {unit}{sanitized}{flagged}"));
        Ok(())
    }

    async fn environment_files(&self, unit: &str) -> Result<Vec<PathBuf>, String> {
        self.record(format!("environment_files {unit}"));
        let mut files = vec![PathBuf::from("/etc/checkout/staging.env")];
        if !self.without_flag_file.contains(&unit) {
            files.push(self.flag.clone());
        }
        Ok(files)
    }

    async fn triggers(&self, unit: &str) -> Result<Vec<String>, String> {
        Ok(vec![unit.replace(".timer", ".service")])
    }

    async fn main_pid(&self, unit: &str) -> Result<u32, String> {
        if self.no_main_pid.contains(&unit) {
            return Ok(0);
        }
        Ok(match unit {
            "checkout.service" => 100,
            "checkout-worker.service" => 101,
            _ => 102,
        })
    }

    async fn group(&self, _unit: &str) -> Result<u32, String> {
        Ok(std::fs::metadata("/proc/self").expect("proc").gid())
    }

    async fn environ(&self, pid: u32) -> Result<Vec<u8>, String> {
        if self.environ_unreadable {
            return Err(format!(
                "cannot read /proc/{pid}/environ: Permission denied"
            ));
        }
        if self.without_flag.contains(&Self::unit_of(pid)) {
            return Ok(b"PATH=/usr/bin\0".to_vec());
        }
        Ok(b"PATH=/usr/bin\0FRAISIER_REFRESHED=1\0".to_vec())
    }
}

const UNITS: &str = "units = [\"checkout-worker.service\", \"checkout-beat.timer\"]\n";
const SKIPPED: &str = "sanitize = \"none\"\nsanitize_reason = \"fixture data only\"\n";

/// A refresh config in `dir`: archives in `<dir>/prod`, the migration's `up`
/// command `up`, `production` and `extra` lines in `[refresh]`, which must
/// declare the sanitize step.
fn config(dir: &Path, production: Option<bool>, up: &str, extra: &str) -> PathBuf {
    std::fs::create_dir_all(dir.join("prod")).expect("mkdir");
    std::fs::create_dir_all(dir.join("etc/checkout")).expect("mkdir");
    std::fs::set_permissions(
        dir.join("etc/checkout"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
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
             [refresh]\nsource_dir = \"{}\"\nsource_environment = \"production\"\n{extra}",
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

/// The flag file a refresh in `dir` writes.
fn flag(dir: &Path) -> PathBuf {
    dir.join("etc/checkout/staging.refreshed.env")
}

/// Units for a refresh in `dir`.
fn units(dir: &Path) -> FakeUnits {
    FakeUnits {
        flag: flag(dir),
        ..FakeUnits::default()
    }
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
            etc: &dir.join("etc"),
            operator_uid: std::fs::metadata("/proc/self").expect("proc").uid(),
            now: now(),
        },
    ))
    .expect("runs");
    std::env::remove_var(DSN_ENV);
    std::env::remove_var(ADMIN_ENV);
    out
}

fn receipt(dir: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.join("state/refreshes/checkout-staging.json"))
        .expect("a receipt");
    serde_json::from_str(&text).expect("json")
}

#[test]
fn an_undeclared_production_key_is_refused_before_anything_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), None, "true", SKIPPED);
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(out.pretty.contains("production = false"), "{}", out.pretty);
    assert!(server.calls().is_empty(), "{:?}", server.calls());
    assert!(units.calls().is_empty(), "{:?}", units.calls());
}

#[test]
fn a_config_whose_service_is_not_systemd_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = config(dir.path(), Some(false), "true", SKIPPED);
    let toml = std::fs::read_to_string(&path).expect("read").replace(
        "adapter = \"systemd\"\nunit = \"checkout.service\"",
        "adapter = \"rc\"\nname = \"checkout\"",
    );
    std::fs::write(&path, toml).expect("write");
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
    let out = refresh(dir.path(), &path, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(out.pretty.contains("systemd"), "{}", out.pretty);
    assert!(units.calls().is_empty(), "{:?}", units.calls());
}

#[test]
fn no_archive_of_the_source_is_refused_before_any_unit_stops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", SKIPPED);
    let (server, units) = (Server::default(), units(dir.path()));
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
    let config = config(
        dir.path(),
        Some(false),
        "true",
        &format!("{SKIPPED}max_age_hours = 24\n"),
    );
    archive(dir.path(), "20261024T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
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
    let extra = format!("{SKIPPED}data_dir = \"{}\"\n", dir.path().display());
    let config = config(dir.path(), Some(false), "true", &extra);
    archive(dir.path(), "20261026T021500Z");
    let server = Server {
        bytes: u64::MAX / 2,
        ..Server::default()
    };
    let units = units(dir.path());
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(units.calls().is_empty(), "{:?}", units.calls());
    assert!(!server.dropped());
}

#[test]
fn without_yes_only_the_plan_is_shown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(
        dir.path(),
        Some(false),
        "true",
        &format!("{SKIPPED}{UNITS}"),
    );
    let newest = archive(dir.path(), "20261026T021500Z");
    archive(dir.path(), "20261025T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
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
    assert!(units.of("stop").is_empty(), "{:?}", units.calls());
    assert!(!server.dropped());
    assert!(!out.pretty.contains("s3cret"), "{}", out.pretty);
}

#[test]
fn every_unit_stops_timers_first_and_starts_again_with_the_flag_proven() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(
        dir.path(),
        Some(false),
        "true",
        &format!("{SKIPPED}{UNITS}"),
    );
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
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
    // Last stopped first, each only once the flag file is written.
    assert_eq!(
        units.of("start"),
        [
            "checkout.service flagged",
            "checkout-worker.service flagged",
            "checkout-beat.timer flagged"
        ]
    );
    assert!(server.dropped());
    assert!(
        server
            .calls()
            .contains(&"set_marker checkout/staging".to_owned()),
        "{:?}",
        server.calls()
    );
    assert_eq!(out.json["service"], "started");
    assert_eq!(
        out.json["outbound_flag"]["proof"],
        serde_json::json!([
            { "unit": "checkout-worker.service", "pid": 101 },
            { "unit": "checkout.service", "pid": 100 }
        ])
    );
    assert!(!out.pretty.contains("s3cret"), "{}", out.pretty);
    assert!(!out.json.to_string().contains("s3cret"), "{}", out.json);
}

#[test]
fn the_flag_file_holds_the_flag_and_only_its_group_may_read_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(
        dir.path(),
        Some(false),
        "true",
        &format!("{SKIPPED}outbound_flag = \"CHECKOUT_QUIET\"\n"),
    );
    archive(dir.path(), "20261026T021500Z");
    let server = Server::default();
    // The fake's processes carry FRAISIER_REFRESHED, not CHECKOUT_QUIET.
    let units = units(dir.path());
    let out = refresh(dir.path(), &config, true, &server, &units);
    let flag = flag(dir.path());
    assert_eq!(
        std::fs::read_to_string(&flag).expect("flag"),
        "CHECKOUT_QUIET=1\n"
    );
    let meta = std::fs::metadata(&flag).expect("meta");
    assert_eq!(meta.mode() & 0o777, 0o640);
    assert_eq!(
        meta.uid(),
        std::fs::metadata("/proc/self").expect("proc").uid()
    );
    // ...and the proof looks for the configured name.
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(out.pretty.contains("CHECKOUT_QUIET=1"), "{}", out.pretty);
}

#[test]
fn a_unit_that_does_not_load_the_flag_file_is_refused_before_anything_stops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(
        dir.path(),
        Some(false),
        "true",
        &format!("{SKIPPED}{UNITS}"),
    );
    archive(dir.path(), "20261026T021500Z");
    let server = Server::default();
    let units = FakeUnits {
        // The timer is checked through the service it triggers.
        without_flag_file: vec!["checkout-beat.service"],
        ..units(dir.path())
    };
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(
        out.pretty.contains("checkout-beat.service"),
        "{}",
        out.pretty
    );
    assert!(
        out.pretty
            .contains(&format!("EnvironmentFile=-{}", flag(dir.path()).display())),
        "{}",
        out.pretty
    );
    assert!(out.pretty.contains("fraisier scaffold"), "{}", out.pretty);
    assert!(units.of("stop").is_empty(), "{:?}", units.calls());
    assert!(!server.dropped());
}

#[test]
fn a_unit_started_without_the_flag_is_stopped_again_and_the_refresh_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(
        dir.path(),
        Some(false),
        "true",
        &format!("{SKIPPED}{UNITS}"),
    );
    archive(dir.path(), "20261026T021500Z");
    let server = Server::default();
    let units = FakeUnits {
        without_flag: vec!["checkout-worker.service"],
        ..units(dir.path())
    };
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert_eq!(out.json["failed_step"], "outbound flag");
    assert!(
        out.pretty
            .contains("checkout-worker.service (pid 101) started without FRAISIER_REFRESHED=1"),
        "{}",
        out.pretty
    );
    // Stopped once before the drop, and every one again after the failed proof.
    assert_eq!(units.of("stop").len(), 6, "{:?}", units.calls());
    assert_eq!(out.json["service"], "stopped");
}

#[test]
fn an_unreadable_environment_fails_the_proof_and_is_never_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", SKIPPED);
    archive(dir.path(), "20261026T021500Z");
    let server = Server::default();
    let units = FakeUnits {
        environ_unreadable: true,
        ..units(dir.path())
    };
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert_eq!(out.json["failed_step"], "outbound flag");
    assert!(out.pretty.contains("Permission denied"), "{}", out.pretty);
    assert!(out.pretty.contains("as root"), "{}", out.pretty);
    assert_eq!(units.of("stop"), ["checkout.service", "checkout.service"]);
}

#[test]
fn a_started_unit_with_no_main_process_fails_the_proof() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", SKIPPED);
    archive(dir.path(), "20261026T021500Z");
    let server = Server::default();
    let units = FakeUnits {
        no_main_pid: vec!["checkout.service"],
        ..units(dir.path())
    };
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(
        out.pretty.contains("no running main process"),
        "{}",
        out.pretty
    );
}

#[test]
fn a_group_writable_flag_directory_is_refused_before_anything_stops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", SKIPPED);
    std::fs::set_permissions(
        dir.path().join("etc/checkout"),
        std::fs::Permissions::from_mode(0o775),
    )
    .expect("chmod");
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(out.pretty.contains("writable"), "{}", out.pretty);
    assert!(units.calls().is_empty(), "{:?}", units.calls());
}

/// A sanitize script in `dir` that dumps its environment to `<dir>/sanitize.env`
/// and exits `code`.
fn sanitize_script(dir: &Path, code: u8) -> PathBuf {
    let path = dir.join("sanitize.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nenv > {}\nexit {code}\n",
            dir.join("sanitize.env").display()
        ),
    )
    .expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    path
}

#[test]
fn sanitize_runs_after_the_migrations_as_the_app_role_before_any_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let step = sanitize_script(dir.path(), 0);
    let migrated = dir.path().join("migrated");
    let up = format!("touch {}", migrated.display());
    let config = config(
        dir.path(),
        Some(false),
        &up,
        &format!("sanitize = \"{}\"\n", step.display()),
    );
    archive(dir.path(), "20261026T021500Z");
    let server = Server::default();
    let units = FakeUnits {
        sanitized: Some(dir.path().join("sanitize.env")),
        ..units(dir.path())
    };
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 0, "{}", out.pretty);
    assert!(migrated.exists());
    assert_eq!(units.of("start"), ["checkout.service sanitized flagged"]);
    let env = std::fs::read_to_string(dir.path().join("sanitize.env")).expect("env");
    assert!(env.contains("PGUSER=app"), "{env}");
    assert!(env.contains("PGDATABASE=checkout_staging"), "{env}");
    assert!(
        !env.contains("r00tpw") && !env.contains(ADMIN_ENV) && !env.contains(DSN_ENV),
        "{env}"
    );
    let receipt = receipt(dir.path());
    assert_eq!(receipt["sanitize"]["outcome"], "passed");
    assert_eq!(
        receipt["sanitize"]["sha256"],
        fraisier_db::sanitize::sha256_hex(&std::fs::read(&step).expect("read"))
    );
}

#[test]
fn a_failed_sanitize_leaves_the_units_stopped_and_writes_no_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let step = sanitize_script(dir.path(), 5);
    let config = config(
        dir.path(),
        Some(false),
        "true",
        &format!("sanitize = \"{}\"\n", step.display()),
    );
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert_eq!(out.json["failed_step"], "sanitize");
    assert!(units.of("start").is_empty(), "{:?}", units.calls());
    assert!(!flag(dir.path()).exists());
    assert_eq!(receipt(dir.path())["sanitize"]["outcome"], "failed");
}

#[test]
fn a_skipped_sanitize_is_in_the_plan_and_the_receipt_with_its_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", SKIPPED);
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
    let plan = refresh(dir.path(), &config, false, &server, &units);
    assert!(
        plan.pretty.contains("sanitize: none (fixture data only)"),
        "{}",
        plan.pretty
    );
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 0, "{}", out.pretty);
    let receipt = receipt(dir.path());
    assert_eq!(receipt["sanitize"]["outcome"], "skipped");
    assert_eq!(receipt["sanitize"]["reason"], "fixture data only");
}

#[test]
fn a_sanitize_step_others_may_write_is_refused_before_anything_stops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let step = sanitize_script(dir.path(), 0);
    std::fs::set_permissions(&step, std::fs::Permissions::from_mode(0o722)).expect("chmod");
    let config = config(
        dir.path(),
        Some(false),
        "true",
        &format!("sanitize = \"{}\"\n", step.display()),
    );
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(out.pretty.contains("writable"), "{}", out.pretty);
    assert!(units.calls().is_empty(), "{:?}", units.calls());
}

#[test]
fn clearing_the_flag_removes_it_and_records_who_and_when() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(dir.path(), Some(false), "true", SKIPPED);
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
    assert_eq!(
        refresh(dir.path(), &config, true, &server, &units).exit_code,
        0
    );
    assert!(flag(dir.path()).exists());
    let out = {
        let _guard = env_guard();
        std::env::set_var("SUDO_USER", "alice");
        let out = clear_flag_with(
            &config,
            &dir.path().join("state"),
            &dir.path().join("etc"),
            now(),
        )
        .expect("runs");
        std::env::remove_var("SUDO_USER");
        out
    };
    assert_eq!(out.exit_code, 0, "{}", out.pretty);
    assert!(!flag(dir.path()).exists());
    assert!(out.pretty.contains("alice"), "{}", out.pretty);
    let cleared = &receipt(dir.path())["flag_cleared"];
    assert_eq!(cleared["by"], "alice");
    assert_eq!(cleared["at"], "20261026T031500Z");
    assert_eq!(cleared["removed"], true);
    // Clearing again is a no-op that says so.
    let again = clear_flag_with(
        &config,
        &dir.path().join("state"),
        &dir.path().join("etc"),
        now(),
    )
    .expect("runs");
    assert_eq!(again.exit_code, 0);
    assert!(
        again.pretty.contains("nothing to clear"),
        "{}",
        again.pretty
    );
    assert!(units.of("start").len() == 1, "clearing never starts a unit");
}

#[test]
fn a_refusal_after_stopping_starts_the_running_units_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(
        dir.path(),
        Some(false),
        "true",
        &format!("{SKIPPED}{UNITS}"),
    );
    archive(dir.path(), "20261026T021500Z");
    let server = Server {
        connections: vec!["app (psql, pid 4242)".to_owned()],
        ..Server::default()
    };
    let units = FakeUnits {
        stopped_already: vec!["checkout-worker.service"],
        ..units(dir.path())
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
    let config = config(dir.path(), Some(false), "true", SKIPPED);
    archive(dir.path(), "20261026T021500Z");
    let server = Server {
        marker: Some("checkout/qa"),
        ..Server::default()
    };
    let units = units(dir.path());
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(out.pretty.contains("checkout/qa"), "{}", out.pretty);
    assert!(!server.dropped());
    assert_eq!(units.of("start"), ["checkout.service"]);
}

#[test]
fn a_failure_after_the_drop_leaves_the_units_stopped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(
        dir.path(),
        Some(false),
        "true",
        &format!("{SKIPPED}{UNITS}"),
    );
    archive(dir.path(), "20261026T021500Z");
    let server = Server {
        restore_fails: true,
        ..Server::default()
    };
    let units = units(dir.path());
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
    let config = config(dir.path(), Some(false), "exit 3", SKIPPED);
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert_eq!(out.json["failed_step"], "migrate");
    assert!(units.of("start").is_empty(), "{:?}", units.calls());
}

#[test]
fn fewer_tables_than_min_tables_fails_the_refresh() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config(
        dir.path(),
        Some(false),
        "true",
        &format!("{SKIPPED}min_tables = 10\n"),
    );
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
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
    let config = config(dir.path(), Some(false), &up, SKIPPED);
    archive(dir.path(), "20261026T021500Z");
    let (server, units) = (Server::default(), units(dir.path()));
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
    let config = config(dir.path(), Some(false), "true", SKIPPED);
    archive(dir.path(), "20261026T021500Z");
    let store = FilesystemStateStore::new(dir.path().join("state")).expect("store");
    let guard = block_on(store.acquire_lock(&FraiseKey::new("checkout", "staging"))).expect("lock");
    let (server, units) = (Server::default(), units(dir.path()));
    let out = refresh(dir.path(), &config, true, &server, &units);
    assert_eq!(out.exit_code, 1, "{}", out.pretty);
    assert!(out.pretty.contains("deploy lock"), "{}", out.pretty);
    assert!(units.of("stop").is_empty(), "{:?}", units.calls());
    assert!(!server.dropped());
    block_on(store.release_lock(guard)).expect("release");
}

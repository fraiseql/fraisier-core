//! `refresh_in_place` against a real PostgreSQL: a role that could not create
//! the target again is refused before the drop, and a failure after the drop
//! names a recovery that works.
//!
//! Ignored unless `FRAISIER_TEST_PG_SUPER_DSN` names a superuser connection to a
//! server whose `pg_hba.conf` trusts local connections for any role (the test
//! connects as the roles it creates), written as `postgres://<user>@<host>/<db>`. Everything it creates is named
//! `fraisier_t_<pid>_*`, and only those are dropped.

use std::process::Stdio;

use fraisier_db::refresh::{marker_value, refresh_in_place, set_marker_sql};
use fraisier_db::restore::{Admin, Postgres, RestoreError, RestoreIdentity, Trigger};
use fraisier_db::{backup_command, psql_command, run, PgConn};

const SUPER_ENV: &str = "FRAISIER_TEST_PG_SUPER_DSN";

struct Fixture {
    dsn: String,
    superuser: PgConn,
    prefix: String,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(case: &str) -> Option<Self> {
        let dsn = std::env::var(SUPER_ENV).ok()?;
        Some(Self {
            superuser: PgConn::parse(&dsn).expect("FRAISIER_TEST_PG_SUPER_DSN parses"),
            dsn,
            prefix: format!("fraisier_t_{}_{case}", std::process::id()),
            dir: tempfile::tempdir().expect("tempdir"),
        })
    }

    fn name(&self, what: &str) -> String {
        format!("{}_{what}", self.prefix)
    }

    /// The same server as `role`, on `database`.
    fn conn(&self, role: &str, database: &str) -> PgConn {
        let host = self
            .dsn
            .split_once('@')
            .and_then(|(_, rest)| rest.split('/').next())
            .unwrap_or_default();
        PgConn::parse(&format!("postgres://{role}@{host}/{database}")).expect("dsn")
    }

    async fn sql(&self, conn: &PgConn, sql: &str) -> String {
        let outcome = run(psql_command(conn, sql)).await.expect("psql runs");
        assert!(outcome.succeeded(), "{sql}: {}", outcome.stderr);
        outcome.stdout
    }

    async fn on_super(&self, database: &str, sql: &str) -> String {
        self.sql(&self.superuser.with_database(database), sql).await
    }

    /// A target owned by `owner`, with one row, marked for `marker`, and a dump
    /// of it to refresh from.
    async fn target(&self, owner: &str, marker: &str) -> (String, std::path::PathBuf) {
        let target = self.name("target");
        self.on_super(
            "postgres",
            &format!("CREATE DATABASE \"{target}\" OWNER \"{owner}\""),
        )
        .await;
        self.on_super(
            &target,
            &format!(
                "CREATE TABLE tb_item (id int); INSERT INTO tb_item VALUES (1); \
                 ALTER TABLE tb_item OWNER TO \"{owner}\""
            ),
        )
        .await;
        self.on_super(
            "postgres",
            &set_marker_sql(&target, marker).expect("marker sql"),
        )
        .await;
        let archive = self.dir.path().join("source.pgdump");
        let dumped = run(backup_command(
            &self.superuser.with_database(&target),
            &archive,
        ))
        .await
        .expect("pg_dump runs");
        assert!(dumped.succeeded(), "{}", dumped.stderr);
        (target, archive)
    }

    async fn rows(&self, target: &str) -> String {
        self.on_super(target, "SELECT count(*) FROM tb_item")
            .await
            .lines()
            .find(|line| line.trim().chars().all(|c| c.is_ascii_digit()) && !line.trim().is_empty())
            .unwrap_or_default()
            .trim()
            .to_owned()
    }
}

/// Drop everything the case created, even when it panicked.
impl Drop for Fixture {
    fn drop(&mut self) {
        let maintenance = self.superuser.with_database("postgres");
        let mut statements = vec![format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name("target")
        )];
        for role in ["app", "admin"].map(|what| self.name(what)) {
            // A parameter grant is a shared dependency DROP ROLE refuses.
            statements.push(format!(
                "REVOKE ALL ON PARAMETER fraisier.refresh_target FROM \"{role}\""
            ));
            statements.push(format!("DROP ROLE IF EXISTS \"{role}\""));
        }
        for sql in statements {
            let _ = psql_command(&maintenance, &sql)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn refused(result: Result<fraisier_db::refresh::Refreshed, RestoreError>) -> String {
    match result {
        Err(RestoreError::Refused(reason)) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a real PostgreSQL superuser via FRAISIER_TEST_PG_SUPER_DSN"]
async fn an_owner_without_createdb_is_refused_and_the_target_survives() {
    let Some(fx) = Fixture::new("nocreate") else {
        return;
    };
    let app = fx.name("app");
    // Everything a refresh needs but CREATEDB, so only that is missing.
    fx.on_super(
        "postgres",
        &format!(
            "CREATE ROLE \"{app}\" LOGIN NOCREATEDB; \
             GRANT SET ON PARAMETER fraisier.refresh_target TO \"{app}\""
        ),
    )
    .await;
    let marker = marker_value("checkout", "staging");
    let (target, archive) = fx.target(&app, &marker).await;

    let identity = RestoreIdentity::new(fx.conn(&app, &target), None, Trigger::Operator);
    let result = refresh_in_place(&Postgres, &identity, &archive, &marker).await;
    let reason = refused(result);
    let rows = fx.rows(&target).await;

    assert!(
        reason.contains(&format!("ALTER ROLE \"{app}\" CREATEDB")),
        "{reason}"
    );
    assert_eq!(rows, "1", "the target and its row survive the refusal");
}

#[tokio::test]
#[ignore = "requires a real PostgreSQL superuser via FRAISIER_TEST_PG_SUPER_DSN"]
async fn an_admin_that_cannot_give_the_target_to_the_app_is_refused() {
    let Some(fx) = Fixture::new("noset") else {
        return;
    };
    let (app, admin) = (fx.name("app"), fx.name("admin"));
    fx.on_super(
        "postgres",
        &format!(
            "CREATE ROLE \"{app}\" LOGIN; CREATE ROLE \"{admin}\" LOGIN CREATEDB; \
             GRANT SET ON PARAMETER fraisier.refresh_target TO \"{admin}\""
        ),
    )
    .await;
    let marker = marker_value("checkout", "staging");
    let (target, archive) = fx.target(&app, &marker).await;
    // The admin may drop the target only as its owner: hand it over, keeping
    // the app as the owner the refresh would give it back to.
    fx.on_super(
        "postgres",
        &format!("ALTER DATABASE \"{target}\" OWNER TO \"{admin}\""),
    )
    .await;

    let identity = RestoreIdentity::new(
        fx.conn(&app, &target),
        Some(Admin {
            env: "CHECKOUT_ADMIN_URL".to_owned(),
            conn: fx.conn(&admin, "postgres"),
        }),
        Trigger::Operator,
    );
    let result = refresh_in_place(&Postgres, &identity, &archive, &marker).await;
    let reason = refused(result);
    let rows = fx.rows(&target).await;

    assert!(
        reason.contains(&format!("GRANT \"{app}\" TO \"{admin}\"")),
        "{reason}"
    );
    assert_eq!(rows, "1", "the target and its row survive the refusal");
}

#[tokio::test]
#[ignore = "requires a real PostgreSQL superuser via FRAISIER_TEST_PG_SUPER_DSN"]
async fn a_failure_after_the_drop_names_a_recovery_that_works() {
    let Some(fx) = Fixture::new("recover") else {
        return;
    };
    let app = fx.name("app");
    fx.on_super(
        "postgres",
        &format!(
            "CREATE ROLE \"{app}\" LOGIN CREATEDB; \
             GRANT SET ON PARAMETER fraisier.refresh_target TO \"{app}\""
        ),
    )
    .await;
    let marker = marker_value("checkout", "staging");
    let (target, archive) = fx.target(&app, &marker).await;
    let identity = RestoreIdentity::new(fx.conn(&app, &target), None, Trigger::Operator);

    // A session on template1 makes CREATE DATABASE fail after the drop.
    let holder_name = fx.name("holder");
    let mut holder = psql_command(
        &fx.superuser.with_database("template1"),
        "SELECT pg_sleep(30)",
    );
    let mut holder = holder
        .env("PGAPPNAME", &holder_name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("psql on template1");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let failed = refresh_in_place(&Postgres, &identity, &archive, &marker).await;
    // Killing psql leaves its backend asleep on template1: end the session.
    fx.on_super(
        "postgres",
        &format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE application_name = '{holder_name}'"
        ),
    )
    .await;
    let _ = holder.wait();
    let reason = match failed {
        Err(RestoreError::Failed(reason)) => reason,
        other => panic!("expected a failure after the drop, got {other:?}"),
    };

    // Follow the recovery literally: create it again as the app's owner, mark
    // it, run the refresh again.
    let createdb = format!("createdb -O {app} {target}");
    assert!(reason.contains(&createdb), "{reason}");
    assert!(
        reason.contains("fraisier db mark-refresh-target"),
        "{reason}"
    );
    let created = run(psql_command(
        &fx.conn(&app, "postgres"),
        &format!("CREATE DATABASE \"{target}\" OWNER \"{app}\""),
    ))
    .await
    .expect("psql runs");
    assert!(created.succeeded(), "{}", created.stderr);
    fx.sql(
        &fx.conn(&app, &target),
        &set_marker_sql(&target, &marker).expect("marker sql"),
    )
    .await;
    let again = refresh_in_place(&Postgres, &identity, &archive, &marker).await;
    let rows = fx.rows(&target).await;

    again.expect("the refresh runs again after the recovery");
    assert_eq!(rows, "1");
}

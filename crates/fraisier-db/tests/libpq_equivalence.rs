//! Each DSN shape reaches, under `PgConn::pg_env`, what `psql "<dsn>"` reaches.
//!
//! Ignored unless `FRAISIER_TEST_PG_SUPER_DSN` names a connection, written as
//! `postgres://<user>@<socket dir, percent-encoded>/<db>`, to a server that
//! listens on that socket and on `localhost` port 5432 and trusts both. The test
//! creates nothing; the DSNs it compares carry no real secret.

use fraisier_db::{psql_command, run, PgConn};

const SUPER_ENV: &str = "FRAISIER_TEST_PG_SUPER_DSN";
const PROBE: &str = "SELECT current_user || '|' || current_database() || '|' || \
                     coalesce(inet_server_port()::text, 'socket') || '|' || \
                     current_setting('application_name')";

/// What a session under `conn`'s environment reports.
async fn under_env(conn: &PgConn) -> String {
    let outcome = run({
        let mut command = psql_command(conn, PROBE);
        command.arg("-tA");
        command
    })
    .await
    .expect("psql runs");
    assert!(
        outcome.succeeded(),
        "{}: {}",
        conn.redacted(),
        outcome.stderr
    );
    outcome.stdout.trim().to_owned()
}

/// What `psql "<dsn>"` reports: libpq reading the URL itself.
async fn under_libpq(dsn: &str) -> String {
    let mut command = std::process::Command::new("psql");
    command.args(["-w", "-X", "-tA", "-c", PROBE, dsn]);
    let outcome = run(command).await.expect("psql runs");
    assert!(outcome.succeeded(), "{dsn}: {}", outcome.stderr);
    outcome.stdout.trim().to_owned()
}

/// The user and the percent-encoded socket directory of the superuser DSN.
fn server() -> Option<(String, String)> {
    let dsn = std::env::var(SUPER_ENV).ok()?;
    let rest = dsn.split_once("://").expect("a URL").1;
    let (user, after) = rest.split_once('@').expect("a user");
    let socket = after.split('/').next().expect("a host");
    Some((user.to_owned(), socket.to_owned()))
}

#[tokio::test]
#[ignore = "requires a real PostgreSQL via FRAISIER_TEST_PG_SUPER_DSN"]
async fn every_shape_reaches_what_libpq_reaches() {
    let Some((user, socket)) = server() else {
        return;
    };
    let shapes = [
        format!("postgresql:///postgres?host={socket}&user={user}"),
        format!("postgresql:///postgres?host={socket}&user={user}&password=pw"),
        format!("postgresql://{user}@{socket}/postgres?port=5432&application_name=x"),
        format!("postgresql://nobody@{socket}/postgres?user={user}&dbname=template1"),
        format!("postgresql://{user}@{socket}/?dbname=postgres&connect_timeout=5"),
        format!("postgresql://{user}@localhost:1/postgres?port=5432"),
        format!("postgresql://{user}@[::1]:5432/postgres?application_name=a%2Bb"),
        format!("postgresql://{user}@127.0.0.1/postgres?options=-c%20application_name%3Dopt"),
    ];
    for dsn in &shapes {
        let conn = PgConn::parse(dsn).unwrap_or_else(|error| panic!("{dsn}: {error}"));
        let ours = under_env(&conn).await;
        let theirs = under_libpq(dsn).await;
        assert_eq!(ours, theirs, "{dsn}");
        // The rendered DSN reaches the same place again.
        assert_eq!(
            under_libpq(&conn.dsn()).await,
            theirs,
            "{}",
            conn.redacted()
        );
    }
}

#[tokio::test]
#[ignore = "requires a real PostgreSQL via FRAISIER_TEST_PG_SUPER_DSN"]
async fn with_database_reaches_the_new_database_over_a_query_dbname() {
    let Some((user, socket)) = server() else {
        return;
    };
    let live = PgConn::parse(&format!(
        "postgresql://{user}@/?host={socket}&dbname=postgres"
    ))
    .expect("parses");
    let copy = live.with_database("template1");
    let expected = format!("{user}|template1|socket|psql");

    assert_eq!(under_env(&copy).await, expected);
    assert_eq!(under_libpq(&copy.dsn()).await, expected);
}

//! `PgConn::parse` reads a URL as libpq does (#105).

use std::collections::BTreeMap;

use super::{DbError, PgConn};

fn env(dsn: &str) -> BTreeMap<String, String> {
    PgConn::parse(dsn)
        .unwrap_or_else(|error| panic!("{dsn}: {error}"))
        .pg_env()
        .into_iter()
        .map(|(k, v)| {
            (
                k.into_string().expect("utf8 key"),
                v.into_string().expect("utf8 val"),
            )
        })
        .collect()
}

fn refusal(dsn: &str) -> String {
    match PgConn::parse(dsn) {
        Ok(conn) => panic!("{dsn} was accepted: {:?}", conn.pg_env()),
        Err(error) => error.to_string(),
    }
}

// ---- the captured #105 shapes ----------------------------------------------

#[test]
fn a_socket_directory_in_the_query_is_the_host() {
    let env = env("postgresql:///db?host=/run/postgresql");
    assert_eq!(env["PGHOST"], "/run/postgresql");
    assert_eq!(env["PGDATABASE"], "db");
}

#[test]
fn a_password_in_the_query_reaches_pgpassword() {
    assert_eq!(
        env("postgresql:///db?host=/run/postgresql&password=pw")["PGPASSWORD"],
        "pw"
    );
    assert_eq!(env("postgresql://u@h/db?password=pw")["PGPASSWORD"], "pw");
}

#[test]
fn port_and_user_in_the_query_are_read() {
    let env = env("postgresql://u@h/db?port=6543&user=other");
    assert_eq!(env["PGPORT"], "6543");
    assert_eq!(env["PGUSER"], "other");
}

#[test]
fn every_keyword_with_a_variable_is_carried() {
    let env = env("postgresql://u@h/db?connect_timeout=5&application_name=x");
    assert_eq!(env["PGCONNECT_TIMEOUT"], "5");
    assert_eq!(env["PGAPPNAME"], "x");
}

#[test]
fn a_keyword_with_no_variable_is_refused_by_name() {
    let message = refusal("postgresql://u@h/db?keepalives=1");
    assert!(message.contains("keepalives"), "{message}");
}

#[test]
fn an_unknown_keyword_is_refused_by_name() {
    let message = refusal("postgresql:///db?bogus_key=1");
    assert!(message.contains("bogus_key"), "{message}");
}

// ---- override order and decoding --------------------------------------------

#[test]
fn the_query_overrides_the_uri_part() {
    let env = env("postgresql://u@h:1/db?host=h2&port=2&user=v&dbname=other");
    assert_eq!(
        (
            env["PGHOST"].as_str(),
            env["PGPORT"].as_str(),
            env["PGUSER"].as_str(),
            env["PGDATABASE"].as_str()
        ),
        ("h2", "2", "v", "other")
    );
}

#[test]
fn a_plus_stays_a_plus() {
    assert_eq!(env("postgresql://u@h/db?password=a+b")["PGPASSWORD"], "a+b");
    assert_eq!(env("postgresql://u:a+b@h/db")["PGPASSWORD"], "a+b");
}

#[test]
fn a_query_value_is_percent_decoded() {
    assert_eq!(env("postgresql:///db?host=%2Ftmp")["PGHOST"], "/tmp");
}

#[test]
fn the_last_repeated_key_wins() {
    assert_eq!(env("postgresql://h/db?port=1&port=2")["PGPORT"], "2");
}

#[test]
fn a_database_named_only_in_the_query_is_enough() {
    assert_eq!(env("postgresql://h/?dbname=shop")["PGDATABASE"], "shop");
}

#[test]
fn ssl_true_means_sslmode_require_as_in_libpq() {
    assert_eq!(env("postgresql://h/db?ssl=true")["PGSSLMODE"], "require");
}

#[test]
fn a_percent_encoded_socket_host_in_the_authority_is_decoded() {
    assert_eq!(
        env("postgresql://u@%2Frun%2Fpostgresql/db")["PGHOST"],
        "/run/postgresql"
    );
}

#[test]
fn an_ipv6_host_loses_its_brackets() {
    let env = env("postgresql://u@[::1]:5433/db");
    assert_eq!(env["PGHOST"], "::1");
    assert_eq!(env["PGPORT"], "5433");
}

// ---- refusals ---------------------------------------------------------------

#[test]
fn a_service_is_refused() {
    let message = refusal("postgresql:///db?service=prod");
    assert!(message.contains("service"), "{message}");
    assert!(!message.contains("prod"), "{message}");
}

#[test]
fn a_key_value_connection_string_is_refused() {
    let message = refusal("host=h dbname=d password=topsecret");
    assert!(!message.contains("topsecret"), "{message}");
}

#[test]
fn a_query_parameter_without_a_value_is_refused_by_name() {
    let message = refusal("postgresql://h/db?sslmode");
    assert!(message.contains("sslmode"), "{message}");
}

#[test]
fn a_refused_keyword_never_echoes_its_value() {
    let message = refusal("postgresql://h/db?sslpassword=hunter2");
    assert!(message.contains("sslpassword"), "{message}");
    assert!(!message.contains("hunter2"), "{message}");
}

#[test]
fn a_malformed_escape_is_refused_without_its_value() {
    let message = refusal("postgresql://u@h/db?password=ab%zzcd");
    assert!(message.contains("password"), "{message}");
    assert!(!message.contains("ab%zz"), "{message}");
}

#[test]
fn an_unencoded_at_in_the_userinfo_is_refused() {
    // libpq splits at the first `@`, a URL parser at the last: two hosts.
    let message = refusal("postgresql://u:p@x@h/db");
    assert!(message.contains("%40"), "{message}");
    assert!(!message.contains("p@x"), "{message}");
}

#[test]
fn another_scheme_is_still_refused_as_one() {
    assert!(matches!(
        PgConn::parse("mysql://u@h/db"),
        Err(DbError::UnsupportedScheme(scheme)) if scheme == "mysql"
    ));
}

#[test]
fn a_dsn_without_a_database_is_still_refused() {
    assert!(matches!(
        PgConn::parse("postgresql://u@h/"),
        Err(DbError::MissingDatabase)
    ));
}

// ---- multi-host -------------------------------------------------------------

#[test]
fn every_multi_host_form_is_recognised() {
    for dsn in [
        "postgresql://h1,h2/db",
        "postgresql://h1:1,h2:2/db",
        "postgresql:///db?host=a,b",
        "postgresql://h/db?hostaddr=192.0.2.1,192.0.2.2",
        "postgresql://h/db?port=1,2",
    ] {
        let conn = PgConn::parse(dsn).unwrap_or_else(|error| panic!("{dsn}: {error}"));
        assert!(conn.is_multi_host(), "{dsn}");
    }
    assert!(!PgConn::parse("postgresql://h:1/db")
        .expect("one host")
        .is_multi_host());
}

#[test]
fn a_multi_host_authority_keeps_each_hosts_port() {
    let env = env("postgresql://h1:1,h2:2/db");
    assert_eq!(env["PGHOST"], "h1,h2");
    assert_eq!(env["PGPORT"], "1,2");
}

// ---- rendering --------------------------------------------------------------

#[test]
fn every_kept_keyword_round_trips_through_dsn() {
    for dsn in [
        "postgresql://u:p%40ss@h:6432/db?sslmode=verify-full&sslrootcert=/etc/ca.pem",
        "postgresql:///db?host=/run/postgresql&password=a+b&application_name=x%26y",
        "postgresql://u@[::1]:5433/db?connect_timeout=5&target_session_attrs=read-write",
        "postgresql://h1:1,h2:2/db?options=-c%20search_path%3Dapp",
    ] {
        let conn = PgConn::parse(dsn).expect("parses");
        let again = PgConn::parse(&conn.dsn())
            .unwrap_or_else(|error| panic!("{} does not parse: {error}", conn.redacted()));
        assert_eq!(again, conn, "{dsn}");
    }
}

#[test]
fn a_socket_host_renders_a_dsn_that_parses() {
    let conn = PgConn::parse("postgresql://lionel@%2Frun%2Fpostgresql/postgres").expect("parses");
    let again = PgConn::parse(&conn.dsn()).expect("the rendered DSN parses");
    assert_eq!(again, conn);
}

#[test]
fn with_database_replaces_a_database_named_in_the_query() {
    let live = PgConn::parse("postgresql://u:pw@h/?dbname=live").expect("parses");
    let copy = live.with_database("live_copy");
    let rendered = copy.dsn();
    assert!(
        !rendered.contains("=live&") && !rendered.ends_with("=live"),
        "{}",
        copy.redacted()
    );
    assert_eq!(
        PgConn::parse(&rendered).expect("parses").database(),
        "live_copy"
    );
}

#[test]
fn redacted_never_shows_a_password_from_either_place() {
    for dsn in [
        "postgresql://u:s3cret@h/db",
        "postgresql://u@h/db?password=s3cret",
    ] {
        let shown = PgConn::parse(dsn).expect("parses").redacted();
        assert!(!shown.contains("s3cret"), "{shown}");
    }
}

#[test]
fn debug_never_shows_the_password() {
    let conn = PgConn::parse("postgresql://u:s3cret@h/db?password=s3cret").expect("parses");
    let shown = format!("{conn:?}");
    assert!(!shown.contains("s3cret"), "{shown}");
}

// ---- resolve: the ambient TLS/GSS policy ------------------------------------

fn ambient_sslmode(name: &str) -> Option<std::ffi::OsString> {
    (name == "PGSSLMODE").then(|| "verify-full".into())
}

#[test]
fn resolve_refuses_an_ambient_tls_policy_the_url_does_not_pin() {
    let error = PgConn::resolve_with("postgresql://u:s3cret@db.internal/app", &ambient_sslmode)
        .expect_err("refused");
    let message = error.to_string();
    assert!(message.contains("PGSSLMODE"), "{message}");
    assert!(message.contains("?sslmode="), "{message}");
    assert!(!message.contains("verify-full"), "{message}");
    assert!(!message.contains("s3cret"), "{message}");
}

#[test]
fn resolve_accepts_what_parse_accepts_when_nothing_is_ambient() {
    let dsn = "postgresql://u@db.internal/app";
    assert_eq!(
        PgConn::resolve_with(dsn, &|_| None).expect("resolves"),
        PgConn::parse(dsn).expect("parses")
    );
}

#[test]
fn parse_stays_pure() {
    assert!(PgConn::parse("postgresql://u@db.internal/app").is_ok());
}

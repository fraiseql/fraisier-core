//! The libpq environment a database child must not inherit.
//!
//! libpq reads a `PG*` variable for every connection parameter the URL leaves
//! unset. An ambient `PGPORT`, `PGOPTIONS` or `PGSSLMODE` left in fraisier's
//! environment would therefore send a child to another server, as another role,
//! into another session, or under another TLS policy than its URL names.
//!
//! - [`scrub_ambient`] removes every libpq variable but [`ALLOWED`], from a
//!   child whose URL fraisier has read with [`crate::PgConn::resolve`]: that
//!   check refuses an ambient TLS or GSS policy the URL does not pin, so
//!   removing it never weakens a connection silently.
//! - [`scrub_gap_fillers`] removes only [`AMBIENT_GAP_FILLERS`], from a child
//!   whose URL fraisier never reads (an IPC adapter). Its TLS and GSS policy
//!   variables still reach it, because nothing checked that its URL pins them.
//!
//! In both, the URL's own values are set afterwards and win.

use std::ffi::OsString;
use std::process::Command;

use crate::uri::KEYWORDS;

/// The variables removed from every database child: they pick a server, a
/// role, a database or a session setting.
pub const AMBIENT_GAP_FILLERS: [&str; 17] = [
    "PGHOST",
    "PGHOSTADDR",
    "PGPORT",
    "PGUSER",
    "PGPASSWORD",
    "PGDATABASE",
    "PGOPTIONS",
    "PGSERVICE",
    "PGSERVICEFILE",
    "PGSYSCONFDIR",
    "PGTZ",
    "PGDATESTYLE",
    "PGGEQO",
    "PGTARGETSESSIONATTRS",
    "PGLOADBALANCEHOSTS",
    "PGCLIENTENCODING",
    "PGREQUIREPEER",
];

/// The ambient variables a resolved child still gets: certificate, key and CRL
/// paths, a password file, and settings that cannot redirect it.
pub const ALLOWED: [&str; 9] = [
    "PGSSLROOTCERT",
    "PGSSLCERT",
    "PGSSLKEY",
    "PGSSLCRL",
    "PGSSLCRLDIR",
    "PGPASSFILE",
    "PGREQUIREAUTH",
    "PGCONNECT_TIMEOUT",
    "PGAPPNAME",
];

/// libpq variables that are no connection keyword.
const NOT_KEYWORDS: [&str; 6] = [
    "PGSERVICEFILE",
    "PGSYSCONFDIR",
    "PGTZ",
    "PGDATESTYLE",
    "PGGEQO",
    "PGLOCALEDIR",
];

/// The TLS and GSS policy variables, each with the URL keyword that pins it.
pub const POLICY: [(&str, &str); 9] = [
    ("PGSSLMODE", "sslmode"),
    ("PGSSLCERTMODE", "sslcertmode"),
    ("PGSSLNEGOTIATION", "sslnegotiation"),
    ("PGSSLMINPROTOCOLVERSION", "ssl_min_protocol_version"),
    ("PGSSLMAXPROTOCOLVERSION", "ssl_max_protocol_version"),
    ("PGGSSENCMODE", "gssencmode"),
    ("PGCHANNELBINDING", "channel_binding"),
    ("PGKRBSRVNAME", "krbsrvname"),
    ("PGGSSLIB", "gsslib"),
];

/// Every libpq variable [`scrub_ambient`] removes.
pub fn withheld() -> impl Iterator<Item = &'static str> {
    KEYWORDS
        .iter()
        .filter_map(|(_, variable)| *variable)
        .chain(NOT_KEYWORDS)
        .filter(|variable| !ALLOWED.contains(variable))
}

/// Remove every [`withheld`] variable from `command`'s environment.
///
/// Only for a child whose URL went through [`crate::PgConn::resolve`]. Call it
/// before setting the connection's own variables: a later `env` of the same
/// name wins.
pub fn scrub_ambient(command: &mut Command) {
    for name in withheld() {
        command.env_remove(name);
    }
}

/// Remove every [`AMBIENT_GAP_FILLERS`] variable from `command`'s environment.
pub fn scrub_gap_fillers(command: &mut Command) {
    for name in AMBIENT_GAP_FILLERS {
        command.env_remove(name);
    }
}

/// The ambient TLS or GSS policy a connection to `conn` would lose: a
/// [`POLICY`] variable `ambient` holds that the URL does not pin, when the
/// connection goes over TCP. A Unix socket uses neither, so it is exempt.
///
/// Names the variable and the keyword to add; never the value.
pub(crate) fn ambient_policy_problem(
    conn: &crate::PgConn,
    ambient: &dyn Fn(&str) -> Option<OsString>,
) -> Option<(&'static str, &'static str)> {
    if conn.is_socket() {
        return None;
    }
    POLICY.into_iter().find(|(variable, keyword)| {
        ambient(variable).is_some_and(|value| !value.is_empty()) && !conn.pins(keyword)
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::ambient_policy_problem;
    use crate::PgConn;

    fn problem(dsn: &str, ambient: &[(&str, &str)]) -> Option<(&'static str, &'static str)> {
        let conn = PgConn::parse(dsn).expect("parses");
        let lookup = |name: &str| {
            ambient
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        };
        ambient_policy_problem(&conn, &lookup)
    }

    #[test]
    fn an_ambient_sslmode_a_tcp_url_does_not_pin_is_a_problem() {
        assert_eq!(
            problem(
                "postgresql://u@db.internal/app",
                &[("PGSSLMODE", "verify-full")]
            ),
            Some(("PGSSLMODE", "sslmode"))
        );
    }

    #[test]
    fn every_policy_variable_is_checked() {
        assert_eq!(
            problem("postgresql://u@h/app", &[("PGGSSENCMODE", "require")]),
            Some(("PGGSSENCMODE", "gssencmode"))
        );
        assert_eq!(
            problem("postgresql://u@h/app", &[("PGCHANNELBINDING", "require")]),
            Some(("PGCHANNELBINDING", "channel_binding"))
        );
    }

    #[test]
    fn a_url_that_pins_the_keyword_is_fine() {
        assert_eq!(
            problem(
                "postgresql://u@h/app?sslmode=disable",
                &[("PGSSLMODE", "verify-full")]
            ),
            None
        );
    }

    #[test]
    fn a_socket_is_exempt() {
        let ambient = [("PGSSLMODE", "verify-full")];
        assert_eq!(problem("postgresql:///app", &ambient), None);
        assert_eq!(
            problem("postgresql:///app?host=/run/postgresql", &ambient),
            None
        );
        assert_eq!(problem("postgresql://u@%2Ftmp,%2Frun/app", &ambient), None);
        assert_eq!(problem("postgresql:///app?host=@abstract", &ambient), None);
    }

    #[test]
    fn a_hostaddr_makes_a_socket_looking_url_tcp() {
        assert_eq!(
            problem(
                "postgresql:///app?host=/run/postgresql&hostaddr=192.0.2.1",
                &[("PGSSLMODE", "require")]
            ),
            Some(("PGSSLMODE", "sslmode"))
        );
    }

    #[test]
    fn one_tcp_host_in_a_list_makes_it_tcp() {
        assert_eq!(
            problem(
                "postgresql:///app?host=/run,db",
                &[("PGSSLMODE", "require")]
            ),
            Some(("PGSSLMODE", "sslmode"))
        );
    }

    #[test]
    fn an_empty_variable_is_unset() {
        assert_eq!(problem("postgresql://u@h/app", &[("PGSSLMODE", "")]), None);
    }
}

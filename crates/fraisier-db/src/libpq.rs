//! The libpq environment a database child must not inherit.
//!
//! libpq reads a `PG*` variable for every connection parameter the URL leaves
//! unset. An ambient `PGPORT`, `PGOPTIONS` or `PGSERVICE` left in fraisier's
//! environment would therefore send a child to another server, as another role,
//! or into another session than its URL names. [`scrub_ambient`] removes those
//! variables; the URL's own values are set afterwards and win.
//!
//! Only the variables that pick a server, a role, a database or a session setting
//! are removed. TLS and GSS variables (`PGSSLMODE`, `PGSSLROOTCERT`, …) and
//! `PGPASSFILE` are left alone: they only tighten or authenticate a connection
//! the URL already chose.

use std::process::Command;

/// The variables removed from every database child.
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

/// Remove every [`AMBIENT_GAP_FILLERS`] variable from `command`'s environment.
///
/// Call it before setting the connection's own variables: a later `env` of the
/// same name wins.
pub fn scrub_ambient(command: &mut Command) {
    for name in AMBIENT_GAP_FILLERS {
        command.env_remove(name);
    }
}

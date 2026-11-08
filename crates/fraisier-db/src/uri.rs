//! Reading a `postgres://` URL the way libpq does, and writing one back.
//!
//! libpq reads every query parameter as a connection keyword, and a query value
//! **overrides** the matching URI part. fraisier hands a connection to its tools
//! through the `PG*` environment alone, so a keyword is kept only when libpq has
//! a variable for it; any other is refused by name rather than dropped.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

use crate::DbError;

/// libpq 18's connection keywords (`PQconndefaults()` of libpq 180006), each
/// with the environment variable that carries it, or `None` when it has none.
pub const KEYWORDS: &[(&str, Option<&str>)] = &[
    ("service", Some("PGSERVICE")),
    ("user", Some("PGUSER")),
    ("password", Some("PGPASSWORD")),
    ("passfile", Some("PGPASSFILE")),
    ("channel_binding", Some("PGCHANNELBINDING")),
    ("connect_timeout", Some("PGCONNECT_TIMEOUT")),
    ("dbname", Some("PGDATABASE")),
    ("host", Some("PGHOST")),
    ("hostaddr", Some("PGHOSTADDR")),
    ("port", Some("PGPORT")),
    ("client_encoding", Some("PGCLIENTENCODING")),
    ("options", Some("PGOPTIONS")),
    ("application_name", Some("PGAPPNAME")),
    ("fallback_application_name", None),
    ("keepalives", None),
    ("keepalives_idle", None),
    ("keepalives_interval", None),
    ("keepalives_count", None),
    ("tcp_user_timeout", None),
    ("sslmode", Some("PGSSLMODE")),
    ("sslnegotiation", Some("PGSSLNEGOTIATION")),
    ("sslcompression", Some("PGSSLCOMPRESSION")),
    ("sslcert", Some("PGSSLCERT")),
    ("sslkey", Some("PGSSLKEY")),
    ("sslcertmode", Some("PGSSLCERTMODE")),
    ("sslpassword", None),
    ("sslrootcert", Some("PGSSLROOTCERT")),
    ("sslcrl", Some("PGSSLCRL")),
    ("sslcrldir", Some("PGSSLCRLDIR")),
    ("sslsni", Some("PGSSLSNI")),
    ("requirepeer", Some("PGREQUIREPEER")),
    ("require_auth", Some("PGREQUIREAUTH")),
    ("min_protocol_version", Some("PGMINPROTOCOLVERSION")),
    ("max_protocol_version", Some("PGMAXPROTOCOLVERSION")),
    ("ssl_min_protocol_version", Some("PGSSLMINPROTOCOLVERSION")),
    ("ssl_max_protocol_version", Some("PGSSLMAXPROTOCOLVERSION")),
    ("gssencmode", Some("PGGSSENCMODE")),
    ("krbsrvname", Some("PGKRBSRVNAME")),
    ("gsslib", Some("PGGSSLIB")),
    ("gssdelegation", Some("PGGSSDELEGATION")),
    ("replication", None),
    ("target_session_attrs", Some("PGTARGETSESSIONATTRS")),
    ("load_balance_hosts", Some("PGLOADBALANCEHOSTS")),
    ("scram_client_key", None),
    ("scram_server_key", None),
    ("oauth_issuer", None),
    ("oauth_client_id", None),
    ("oauth_client_secret", None),
    ("oauth_scope", None),
    ("sslkeylogfile", None),
];

/// Why a query parameter was refused. The key is named; its value never is.
const UNKNOWN: &str = "is not a libpq connection keyword";
const NO_VARIABLE: &str = "has no libpq environment variable, and fraisier hands every \
     connection to its tools through the PG* environment alone, so it cannot pass it on";
const SERVICE: &str = "names a service-file entry, which can supply another host, role \
     or password than the DSN shows; write those in the DSN instead";
const NO_VALUE: &str = "has no value";
const SSL_ALIAS: &str = "accepts only `true` (libpq's alias for sslmode=require)";

/// The keyword `key` names, with the variable that carries it.
fn keyword(key: &str) -> Result<(&'static str, &'static str), DbError> {
    let refuse = |reason| DbError::UnsupportedKeyword {
        key: key.to_owned(),
        reason,
    };
    let (name, variable) = KEYWORDS
        .iter()
        .find(|(name, _)| *name == key)
        .ok_or_else(|| refuse(UNKNOWN))?;
    if *name == "service" {
        return Err(refuse(SERVICE));
    }
    variable
        .map(|variable| (*name, variable))
        .ok_or_else(|| refuse(NO_VARIABLE))
}

/// The environment variable carrying `keyword`, one [`parse`] kept.
pub fn variable(keyword: &str) -> &'static str {
    KEYWORDS
        .iter()
        .find_map(|(name, variable)| (*name == keyword).then_some(*variable).flatten())
        .unwrap_or_else(|| unreachable!("parse keeps only keywords with a variable"))
}

/// Decode `%XX` escapes as libpq does: `+` stays a `+`, a bad escape or `%00`
/// is an error, and the result must be UTF-8. `what` names the component in
/// the error; the text itself is never echoed.
fn decode(text: &str, what: &str) -> Result<String, DbError> {
    let invalid = || DbError::Parse(format!("the DSN's {what} holds an invalid percent-escape"));
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let byte = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .filter(|byte| *byte != 0)
                .ok_or_else(invalid)?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| DbError::Parse(format!("the DSN's {what} is not UTF-8")))
}

/// Parse `dsn` into libpq keywords and their values.
///
/// # Errors
/// See [`crate::PgConn::parse`].
pub fn parse(dsn: &str) -> Result<BTreeMap<&'static str, String>, DbError> {
    let Some((scheme, rest)) = dsn.split_once("://") else {
        return Err(DbError::Parse(
            "the DSN is not a postgres:// URL (a key=value connection string is not supported)"
                .to_owned(),
        ));
    };
    if scheme != "postgres" && scheme != "postgresql" {
        let valid = scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
        return Err(if valid && !scheme.is_empty() {
            DbError::UnsupportedScheme(scheme.to_owned())
        } else {
            DbError::Parse("the DSN is not a postgres:// URL".to_owned())
        });
    }
    let (before_query, query) = rest
        .split_once('?')
        .map_or((rest, None), |(a, q)| (a, Some(q)));
    let (authority, path) = before_query
        .split_once('/')
        .map_or((before_query, None), |(a, p)| (a, Some(p)));

    let mut params = BTreeMap::new();
    let hosts = match authority.split_once('@') {
        Some((_, hosts)) if hosts.contains('@') => {
            return Err(DbError::Parse(
                "the DSN's user name or password holds an unencoded '@': libpq and a URL \
                 parser would read different hosts from it; percent-encode it as %40"
                    .to_owned(),
            ));
        }
        Some((userinfo, hosts)) => {
            let (user, password) = userinfo
                .split_once(':')
                .map_or((userinfo, None), |(u, p)| (u, Some(p)));
            if !user.is_empty() {
                params.insert("user", decode(user, "user name")?);
            }
            if let Some(password) = password {
                params.insert("password", decode(password, "password")?);
            }
            hosts
        }
        None => authority,
    };
    read_hosts(hosts, &mut params)?;
    if let Some(path) = path.filter(|path| !path.is_empty()) {
        params.insert("dbname", decode(path, "database name")?);
    }
    for pair in query
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let Some((key, value)) = pair.split_once('=') else {
            let known = KEYWORDS.iter().any(|(name, _)| *name == pair);
            return Err(if known {
                DbError::UnsupportedKeyword {
                    key: pair.to_owned(),
                    reason: NO_VALUE,
                }
            } else {
                DbError::Parse("a DSN query parameter has no `=`".to_owned())
            });
        };
        let key = decode(key, "query")?;
        let value = decode(value, &format!("`{key}` parameter"))?;
        if key == "ssl" {
            if value != "true" {
                return Err(DbError::UnsupportedKeyword {
                    key,
                    reason: SSL_ALIAS,
                });
            }
            params.insert("sslmode", "require".to_owned());
            continue;
        }
        let (name, _) = keyword(&key)?;
        params.insert(name, value);
    }
    if params.get("dbname").is_none_or(String::is_empty) {
        return Err(DbError::MissingDatabase);
    }
    Ok(params)
}

/// Read a URL's host list (`h1:p1,[::1]:p2,…`) into `host` and `port`, the way
/// libpq joins them: one comma-separated entry per host.
fn read_hosts(hosts: &str, params: &mut BTreeMap<&'static str, String>) -> Result<(), DbError> {
    if hosts.is_empty() {
        return Ok(());
    }
    let mut names = Vec::new();
    let mut ports = Vec::new();
    for entry in hosts.split(',') {
        let (name, port) = if let Some(bracketed) = entry.strip_prefix('[') {
            let (name, after) = bracketed
                .split_once(']')
                .ok_or_else(|| DbError::Parse("the DSN's IPv6 host has no `]`".to_owned()))?;
            let port = match after {
                "" => "",
                _ => after.strip_prefix(':').ok_or_else(|| {
                    DbError::Parse("the DSN's IPv6 host is followed by more than a port".to_owned())
                })?,
            };
            (name.to_owned(), port)
        } else {
            let (name, port) = entry.split_once(':').unwrap_or((entry, ""));
            (decode(name, "host")?, port)
        };
        if !port.is_empty() && port.parse::<u16>().is_err() {
            return Err(DbError::Parse("the DSN's port is not a number".to_owned()));
        }
        names.push(name);
        ports.push(port);
    }
    if names.iter().any(|name| !name.is_empty()) {
        params.insert("host", names.join(","));
    }
    if ports.iter().any(|port| !port.is_empty()) {
        params.insert("port", ports.join(","));
    }
    Ok(())
}

/// Characters escaped in the userinfo, a single host and the database path.
const COMPONENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// Characters escaped in a query value: as [`COMPONENT`], but a `,` (the host
/// and port list separator) and a `/` stay readable.
const QUERY_VALUE: &AsciiSet = &COMPONENT.remove(b',').remove(b'/');

fn encode(text: &str, set: &'static AsciiSet) -> String {
    utf8_percent_encode(text, set).to_string()
}

/// Render `params` as a `postgres://` URL [`parse`] reads back to the same
/// keywords; without the password when `with_password` is false.
pub fn render(params: &BTreeMap<&'static str, String>, with_password: bool) -> String {
    let mut out = String::from("postgres://");
    let password = params.get("password").filter(|_| with_password);
    if let Some(user) = params.get("user") {
        out.push_str(&encode(user, COMPONENT));
    }
    if let Some(password) = password {
        out.push(':');
        out.push_str(&encode(password, COMPONENT));
    }
    if params.contains_key("user") || password.is_some() {
        out.push('@');
    }
    let host = params.get("host");
    let port = params.get("port");
    let single = !host.is_some_and(|h| h.contains(',')) && !port.is_some_and(|p| p.contains(','));
    if single {
        if let Some(host) = host {
            if host.contains(':') {
                let _ = write!(out, "[{host}]");
            } else {
                out.push_str(&encode(host, COMPONENT));
            }
        }
        if let Some(port) = port {
            let _ = write!(out, ":{port}");
        }
    }
    out.push('/');
    out.push_str(&encode(&params["dbname"], COMPONENT));
    let mut separator = '?';
    for (key, value) in params {
        let in_authority = matches!(*key, "user" | "password" | "dbname")
            || (single && matches!(*key, "host" | "port"));
        if in_authority {
            continue;
        }
        let _ = write!(out, "{separator}{key}={}", encode(value, QUERY_VALUE));
        separator = '&';
    }
    out
}

#[cfg(test)]
mod tests {
    use super::KEYWORDS;

    /// `PQconndefaults()` of libpq 180006, one `keyword variable` (or `-`) a line.
    const CAPTURED: &str = include_str!("uri/libpq18-conndefaults.txt");

    #[test]
    fn the_keyword_table_is_libpq_18s() {
        let captured: Vec<(&str, Option<&str>)> = CAPTURED
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .map(|line| {
                let (keyword, variable) = line.split_once(' ').expect("keyword variable");
                (keyword, (variable != "-").then_some(variable))
            })
            .collect();
        assert_eq!(KEYWORDS, captured.as_slice());
    }
}

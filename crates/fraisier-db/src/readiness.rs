//! Is this PostgreSQL server ready for what the deployable declares it needs?
//!
//! A deployable can declare a PostgreSQL major-version floor and that it needs
//! pg_tviews. fraisier never installs either (that is the operator's documented
//! job); it checks, and refuses a target that is not ready with the exact reason.
//!
//! Every check fails closed. A fact the probe cannot read is a refusal that says
//! what could not be read, never "absent, so skip": a deployable that declared
//! pg_tviews must not deploy onto a server nobody could inspect.
//!
//! pg_tviews is spoken to only through its read contract 2 (`contract_version()`
//! in the extension's own schema, read from the catalogue rather than assumed).
//! `pg_extension.extversion` is no help: it reads `0.1.0` on every beta.

use std::fmt;
use std::path::Path;
use std::process::Command;

use crate::PgConn;

/// The pg_tviews read contract this fraisier speaks.
pub const TVIEWS_CONTRACT: i64 = 2;

/// What a deployable declares it needs from its database server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Requirements {
    /// The lowest PostgreSQL major version the server may run (`18` admits 18.x
    /// and later).
    pub postgres_major: Option<u32>,
    /// Whether pg_tviews must be preloaded, installed and on contract
    /// [`TVIEWS_CONTRACT`].
    pub pg_tviews: bool,
}

/// What the probe read from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// `server_version_num` (e.g. `180004` for 18.4).
    pub server_version_num: u32,
    /// `shared_preload_libraries`, or `None` when this role may not read it.
    pub shared_preload_libraries: Option<String>,
    /// The schema pg_tviews is installed in, or `None` when it is not installed.
    pub tviews_schema: Option<String>,
    /// The contract `contract_version()` reported, when it was asked.
    pub tviews_contract: Option<Result<i64, String>>,
}

/// One reason the server is not ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unready {
    /// The server runs an older major version than the floor.
    ServerTooOld {
        /// The declared floor.
        required_major: u32,
        /// The server's `server_version_num`.
        server_version_num: u32,
    },
    /// This role may not read `shared_preload_libraries`.
    PreloadUnreadable,
    /// `pg_tviews` is not in `shared_preload_libraries`.
    PreloadMissing {
        /// The value that was read.
        actual: String,
    },
    /// The pg_tviews extension is not installed in this database.
    ExtensionAbsent,
    /// `contract_version()` could not be called.
    ContractUnreadable {
        /// What the server said.
        detail: String,
    },
    /// pg_tviews offers an older contract than this fraisier speaks.
    ContractOutdated {
        /// The contract it offers.
        actual: i64,
    },
    /// pg_tviews offers a newer contract than this fraisier speaks.
    ContractNewer {
        /// The contract it offers.
        actual: i64,
    },
}

impl fmt::Display for Unready {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServerTooOld {
                required_major,
                server_version_num,
            } => write!(
                f,
                "the server runs PostgreSQL {}.{}, but this deployable requires {required_major} \
                 or later",
                server_version_num / 10_000,
                server_version_num % 10_000
            ),
            Self::PreloadUnreadable => f.write_str(
                "this role cannot read shared_preload_libraries, so whether pg_tviews is \
                 preloaded is unknown; grant it pg_read_all_settings",
            ),
            Self::PreloadMissing { actual } => write!(
                f,
                "pg_tviews is not in shared_preload_libraries (it is '{actual}'); add it and \
                 restart PostgreSQL, or CREATE TABLE tv_… makes plain tables"
            ),
            Self::ExtensionAbsent => f.write_str(
                "the pg_tviews extension is not installed in this database; a superuser must \
                 run CREATE EXTENSION pg_tviews",
            ),
            Self::ContractUnreadable { detail } => write!(
                f,
                "pg_tviews is installed but contract_version() could not be read ({detail}); \
                 a pg_tviews older than 0.1.0-beta.30 has none, so upgrade pg_tviews, or grant \
                 this role USAGE on its schema"
            ),
            Self::ContractOutdated { actual } => write!(
                f,
                "pg_tviews offers read contract {actual}, but this fraisier speaks contract \
                 {TVIEWS_CONTRACT}; upgrade pg_tviews"
            ),
            Self::ContractNewer { actual } => write!(
                f,
                "pg_tviews offers read contract {actual}, but this fraisier speaks contract \
                 {TVIEWS_CONTRACT}; upgrade fraisier"
            ),
        }
    }
}

/// Every reason `observed` does not meet `requirements`, in check order. Empty
/// means ready.
#[must_use]
pub fn assess(requirements: &Requirements, observed: &Observed) -> Vec<Unready> {
    let mut unready = Vec::new();
    if let Some(required_major) = requirements.postgres_major {
        if observed.server_version_num / 10_000 < required_major {
            unready.push(Unready::ServerTooOld {
                required_major,
                server_version_num: observed.server_version_num,
            });
        }
    }
    if !requirements.pg_tviews {
        return unready;
    }
    match &observed.shared_preload_libraries {
        None => unready.push(Unready::PreloadUnreadable),
        Some(value) if !preloads_tviews(value) => unready.push(Unready::PreloadMissing {
            actual: value.clone(),
        }),
        Some(_) => {}
    }
    if observed.tviews_schema.is_none() {
        unready.push(Unready::ExtensionAbsent);
        return unready;
    }
    match &observed.tviews_contract {
        None => unready.push(Unready::ContractUnreadable {
            detail: "it was not asked".to_owned(),
        }),
        Some(Err(detail)) => unready.push(Unready::ContractUnreadable {
            detail: detail.clone(),
        }),
        Some(Ok(actual)) if *actual < TVIEWS_CONTRACT => {
            unready.push(Unready::ContractOutdated { actual: *actual });
        }
        Some(Ok(actual)) if *actual > TVIEWS_CONTRACT => {
            unready.push(Unready::ContractNewer { actual: *actual });
        }
        Some(Ok(_)) => {}
    }
    unready
}

/// Whether `pg_tviews` is one of the libraries in a `shared_preload_libraries`
/// value (comma-separated, optionally quoted, optionally a `$libdir/` path).
#[must_use]
pub fn preloads_tviews(shared_preload_libraries: &str) -> bool {
    shared_preload_libraries.split(',').any(|entry| {
        let entry = entry.trim().trim_matches('"');
        entry.rsplit('/').next() == Some("pg_tviews")
    })
}

/// The SQL the first probe call runs: one JSON object holding every fact that
/// can be read without risking an error.
///
/// `shared_preload_libraries` is read through `pg_settings`, which hides a
/// superuser-only setting from a role that may not read it rather than raising,
/// so "cannot read" arrives as a `null` and never aborts the probe.
pub const PROBE_SQL: &str = "\
SELECT json_build_object(\
'server_version_num', current_setting('server_version_num')::int, \
'shared_preload_libraries', \
(SELECT setting FROM pg_settings WHERE name = 'shared_preload_libraries'), \
'tviews_schema', \
(SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace \
WHERE e.extname = 'pg_tviews'))";

/// Parse the first probe call's output.
///
/// # Errors
/// The output is not the JSON object [`PROBE_SQL`] produces.
pub fn parse_probe(stdout: &str) -> Result<Observed, String> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("the readiness probe did not print JSON: {error}"))?;
    let text = |key: &str| -> Result<Option<String>, String> {
        match &value[key] {
            serde_json::Value::Null => Ok(None),
            serde_json::Value::String(text) => Ok(Some(text.clone())),
            other => Err(format!("the readiness probe's {key} is {other}, not text")),
        }
    };
    let server_version_num = value["server_version_num"]
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| {
            format!(
                "the readiness probe's server_version_num is {}, not a number",
                value["server_version_num"]
            )
        })?;
    Ok(Observed {
        server_version_num,
        shared_preload_libraries: text("shared_preload_libraries")?,
        tviews_schema: text("tviews_schema")?,
        tviews_contract: None,
    })
}

/// The SQL that asks pg_tviews, installed in `schema`, for its contract.
#[must_use]
pub fn contract_sql(schema: &str) -> String {
    format!(
        "SELECT \"{}\".contract_version()",
        schema.replace('"', "\"\"")
    )
}

/// A `psql` that runs `sql` and prints only the bare result (`-tA`).
#[must_use]
pub fn query_command(psql: &Path, conn: &PgConn, sql: &str) -> Command {
    let mut command = Command::new(psql);
    command.envs(conn.pg_env());
    command
        .arg("-w")
        .arg("-X")
        .arg("-v")
        .arg("ON_ERROR_STOP=1")
        .arg("-tA")
        .arg("-c")
        .arg(sql);
    command
}

/// Read every fact [`assess`] needs from the server behind `conn`, with `psql`.
///
/// `contract_version()` is asked only when pg_tviews is installed, in a call of
/// its own, so a missing or forbidden function is recorded as an unreadable
/// contract rather than failing the whole probe.
///
/// # Errors
/// `psql` could not run, the first query failed (for example the server is
/// unreachable), or its output was not understood.
pub async fn observe(psql: &Path, conn: &PgConn) -> Result<Observed, String> {
    let outcome = crate::run(query_command(psql, conn, PROBE_SQL))
        .await
        .map_err(|error| error.to_string())?;
    if !outcome.succeeded() {
        return Err(format!(
            "the readiness probe failed: {}",
            outcome.stderr.trim()
        ));
    }
    let mut observed = parse_probe(&outcome.stdout)?;
    if let Some(schema) = &observed.tviews_schema {
        let contract = crate::run(query_command(psql, conn, &contract_sql(schema)))
            .await
            .map_err(|error| error.to_string())?;
        observed.tviews_contract = Some(if contract.succeeded() {
            contract.stdout.trim().parse::<i64>().map_err(|_| {
                format!(
                    "contract_version() returned '{}', not a number",
                    contract.stdout.trim()
                )
            })
        } else {
            // psql's first line is the error; the rest is LINE/HINT decoration.
            Err(contract
                .stderr
                .trim()
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned())
        });
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::{
        assess, contract_sql, parse_probe, preloads_tviews, query_command, Observed, Requirements,
        Unready, PROBE_SQL,
    };
    use crate::PgConn;

    const PG18: Requirements = Requirements {
        postgres_major: Some(18),
        pg_tviews: false,
    };

    const TVIEWS: Requirements = Requirements {
        postgres_major: Some(18),
        pg_tviews: true,
    };

    fn ready() -> Observed {
        Observed {
            server_version_num: 180_004,
            shared_preload_libraries: Some("pg_stat_statements,pg_tviews".to_owned()),
            tviews_schema: Some("tviews".to_owned()),
            tviews_contract: Some(Ok(2)),
        }
    }

    #[test]
    fn a_ready_server_has_no_refusals() {
        assert_eq!(assess(&TVIEWS, &ready()), Vec::new());
    }

    #[test]
    fn nothing_declared_refuses_nothing() {
        let observed = Observed {
            server_version_num: 120_000,
            shared_preload_libraries: None,
            tviews_schema: None,
            tviews_contract: None,
        };
        assert_eq!(assess(&Requirements::default(), &observed), Vec::new());
    }

    #[test]
    fn a_17_server_is_refused_under_an_18_floor() {
        let observed = Observed {
            server_version_num: 170_005,
            ..ready()
        };
        assert_eq!(
            assess(&PG18, &observed),
            [Unready::ServerTooOld {
                required_major: 18,
                server_version_num: 170_005
            }]
        );
    }

    #[test]
    fn exactly_18_0_is_admitted() {
        let observed = Observed {
            server_version_num: 180_000,
            ..ready()
        };
        assert_eq!(assess(&PG18, &observed), Vec::new());
    }

    #[test]
    fn a_newer_major_is_admitted() {
        let observed = Observed {
            server_version_num: 190_001,
            ..ready()
        };
        assert_eq!(assess(&PG18, &observed), Vec::new());
    }

    #[test]
    fn a_missing_preload_is_refused_naming_what_is_there() {
        let observed = Observed {
            shared_preload_libraries: Some("pg_stat_statements".to_owned()),
            ..ready()
        };
        assert_eq!(
            assess(&TVIEWS, &observed),
            [Unready::PreloadMissing {
                actual: "pg_stat_statements".to_owned()
            }]
        );
    }

    #[test]
    fn an_unreadable_preload_is_refused_not_skipped() {
        let observed = Observed {
            shared_preload_libraries: None,
            ..ready()
        };
        assert_eq!(assess(&TVIEWS, &observed), [Unready::PreloadUnreadable]);
    }

    #[test]
    fn an_absent_extension_is_refused() {
        let observed = Observed {
            tviews_schema: None,
            tviews_contract: None,
            ..ready()
        };
        assert_eq!(assess(&TVIEWS, &observed), [Unready::ExtensionAbsent]);
    }

    #[test]
    fn contract_one_is_outdated() {
        let observed = Observed {
            tviews_contract: Some(Ok(1)),
            ..ready()
        };
        assert_eq!(
            assess(&TVIEWS, &observed),
            [Unready::ContractOutdated { actual: 1 }]
        );
    }

    #[test]
    fn contract_three_is_newer_not_outdated() {
        let observed = Observed {
            tviews_contract: Some(Ok(3)),
            ..ready()
        };
        assert_eq!(
            assess(&TVIEWS, &observed),
            [Unready::ContractNewer { actual: 3 }]
        );
    }

    #[test]
    fn a_contract_call_that_failed_is_refused_with_the_servers_words() {
        let observed = Observed {
            tviews_contract: Some(Err("ERROR:  permission denied for schema tviews".to_owned())),
            ..ready()
        };
        assert_eq!(
            assess(&TVIEWS, &observed),
            [Unready::ContractUnreadable {
                detail: "ERROR:  permission denied for schema tviews".to_owned()
            }]
        );
    }

    #[test]
    fn an_installed_extension_whose_contract_was_never_asked_is_refused() {
        // Fail closed: "not asked" is not "ok".
        let observed = Observed {
            tviews_contract: None,
            ..ready()
        };
        assert!(
            matches!(
                assess(&TVIEWS, &observed).as_slice(),
                [Unready::ContractUnreadable { .. }]
            ),
            "{:?}",
            assess(&TVIEWS, &observed)
        );
    }

    #[test]
    fn every_reason_is_reported_not_only_the_first() {
        let observed = Observed {
            server_version_num: 160_000,
            shared_preload_libraries: Some(String::new()),
            tviews_schema: None,
            tviews_contract: None,
        };
        assert_eq!(
            assess(&TVIEWS, &observed),
            [
                Unready::ServerTooOld {
                    required_major: 18,
                    server_version_num: 160_000
                },
                Unready::PreloadMissing {
                    actual: String::new()
                },
                Unready::ExtensionAbsent,
            ]
        );
    }

    #[test]
    fn preload_lists_are_read_the_way_postgres_writes_them() {
        for value in [
            "pg_tviews",
            "pg_stat_statements,pg_tviews",
            "pg_stat_statements, pg_tviews",
            "\"pg_tviews\"",
            "$libdir/pg_tviews",
        ] {
            assert!(preloads_tviews(value), "{value:?} preloads pg_tviews");
        }
        for value in ["", "pg_stat_statements", "pg_tviews_old", "not_pg_tviews"] {
            assert!(!preloads_tviews(value), "{value:?} does not");
        }
    }

    #[test]
    fn every_reason_names_the_expected_and_the_actual_value() {
        let cases = [
            (
                Unready::ServerTooOld {
                    required_major: 18,
                    server_version_num: 170_005,
                },
                &["18", "17.5"][..],
            ),
            (Unready::PreloadUnreadable, &["pg_read_all_settings"][..]),
            (
                Unready::PreloadMissing {
                    actual: "pg_stat_statements".to_owned(),
                },
                &["shared_preload_libraries", "pg_stat_statements", "restart"][..],
            ),
            (
                Unready::ExtensionAbsent,
                &["CREATE EXTENSION pg_tviews"][..],
            ),
            (
                Unready::ContractOutdated { actual: 1 },
                &["contract 1", "contract 2", "upgrade pg_tviews"][..],
            ),
            (
                Unready::ContractNewer { actual: 3 },
                &["contract 3", "contract 2", "upgrade fraisier"][..],
            ),
            (
                Unready::ContractUnreadable {
                    detail: "permission denied".to_owned(),
                },
                &["contract_version()", "permission denied"][..],
            ),
        ];
        for (reason, words) in cases {
            let text = reason.to_string();
            for word in words {
                assert!(text.contains(word), "{text:?} should mention {word:?}");
            }
        }
    }

    #[test]
    fn the_probe_output_parses_including_nulls() {
        assert_eq!(
            parse_probe(
                "{\"server_version_num\" : 180004, \"shared_preload_libraries\" : null, \
                 \"tviews_schema\" : null}\n"
            ),
            Ok(Observed {
                server_version_num: 180_004,
                shared_preload_libraries: None,
                tviews_schema: None,
                tviews_contract: None,
            })
        );
    }

    #[test]
    fn probe_output_that_is_not_the_object_is_an_error() {
        assert!(parse_probe("").is_err());
        assert!(parse_probe("{\"server_version_num\": \"x\"}").is_err());
        assert!(parse_probe("not json").is_err());
    }

    #[test]
    fn the_contract_query_quotes_the_schema() {
        assert_eq!(
            contract_sql("tviews"),
            "SELECT \"tviews\".contract_version()"
        );
        assert_eq!(
            contract_sql("we\"ird"),
            "SELECT \"we\"\"ird\".contract_version()"
        );
    }

    #[test]
    fn the_probe_puts_no_secret_on_argv() {
        let conn = PgConn::parse("postgres://app:s3cret@db/shop").expect("dsn");
        let command = query_command(std::path::Path::new("psql"), &conn, PROBE_SQL);
        let argv: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(argv.iter().all(|arg| !arg.contains("s3cret")), "{argv:?}");
        assert!(argv.iter().any(|arg| arg == "-tA"), "{argv:?}");
        assert!(argv.iter().any(|arg| arg == PROBE_SQL), "{argv:?}");
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "PGPASSWORD"
                    && value.is_some_and(|value| value == "s3cret")),
            "the password travels in PGPASSWORD"
        );
    }

    /// A real server: `FRAISIER_READINESS_TEST_DSN` names any database. Read-only.
    #[tokio::test]
    #[ignore = "requires a real Postgres reachable via FRAISIER_READINESS_TEST_DSN"]
    async fn a_real_server_answers_the_probe() {
        let dsn = std::env::var("FRAISIER_READINESS_TEST_DSN").expect("dsn");
        let conn = PgConn::parse(&dsn).expect("dsn");
        let outcome = crate::run(query_command(
            std::path::Path::new("psql"),
            &conn,
            PROBE_SQL,
        ))
        .await
        .expect("psql runs");
        assert!(outcome.succeeded(), "{}", outcome.stderr);
        let observed = parse_probe(&outcome.stdout).expect("parses");
        assert!(observed.server_version_num >= 100_000, "{observed:?}");
        let observed = super::observe(std::path::Path::new("psql"), &conn)
            .await
            .expect("observes");
        // An installed extension is always asked for its contract.
        assert_eq!(
            observed.tviews_schema.is_some(),
            observed.tviews_contract.is_some(),
            "{observed:?}"
        );
        eprintln!("observed: {observed:?}");
        eprintln!(
            "assessed: {:?}",
            super::assess(
                &Requirements {
                    postgres_major: Some(18),
                    pg_tviews: true
                },
                &observed
            )
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
        );
    }
}

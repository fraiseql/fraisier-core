//! The pg_tviews data surface: which TVIEWs are empty under a populated view,
//! who may rebuild them, and the rebuild itself.
//!
//! Everything goes through pg_tviews' read contract 2 (the `registry` view)
//! and its documented `pg_tviews_rebuild_all`, in the schema the extension was
//! installed in, read from the catalog. Nothing is inferred from a name: the
//! registry carries each TVIEW's backing view as a `regclass`.
//!
//! Each query prints one JSON value (`psql -tA`), so a reply is parsed as data,
//! never scraped.

use std::path::Path;

use serde::Deserialize;

use crate::readiness::query_command;
use crate::PgConn;

/// One registered TVIEW and its backing view, and whether the connecting role
/// may read both.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Tview {
    /// The TVIEW's schema.
    pub schema: String,
    /// The TVIEW's table name.
    pub name: String,
    /// The backing view's schema.
    pub view_schema: String,
    /// The backing view's name.
    pub view_name: String,
    /// Whether the connecting role holds `SELECT` on both.
    pub readable: bool,
}

impl Tview {
    /// `schema.name`, for messages.
    #[must_use]
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    /// `view_schema.view_name`, for messages.
    #[must_use]
    pub fn view(&self) -> String {
        format!("{}.{}", self.view_schema, self.view_name)
    }
}

/// The registry as the connecting role sees it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Registry {
    /// `current_user`, for the GRANTs a refusal names.
    pub user: String,
    /// Every registered TVIEW, sorted by schema and name.
    pub tviews: Vec<Tview>,
}

/// What the empty-TVIEW probe found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Emptiness {
    /// TVIEWs with no rows whose backing view has some.
    pub empty: Vec<Tview>,
    /// TVIEWs the role cannot read, each with the GRANT that fixes it.
    pub unreadable: Vec<(Tview, String)>,
}

/// One entity `pg_tviews_rebuild_all` filled, and the rows it inserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuilt {
    /// The pg_tviews entity name.
    pub entity: String,
    /// The rows inserted (`rows_inserted`; `rows` before pg_tviews beta.29).
    pub rows: i64,
}

/// Who the connection is, as the restore prechecks and `doctor` ask.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Identity {
    /// `current_user`.
    pub user: String,
    /// Whether that role is a superuser.
    pub superuser: bool,
    /// Whether the server is a standby (or still recovering).
    pub recovery: bool,
    /// The cluster's `pg_control_system()` identifier: two connections reach
    /// the same cluster exactly when it matches.
    pub system_identifier: String,
}

/// `"name"`, with embedded quotes doubled.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The SQL listing `schema`'s registry with each TVIEW's backing view.
#[must_use]
pub fn registry_sql(schema: &str) -> String {
    format!(
        "SELECT json_build_object('user', current_user, 'tviews', COALESCE((\
         SELECT json_agg(json_build_object(\
         'schema', r.schema, 'name', r.name, \
         'view_schema', vn.nspname, 'view_name', vc.relname, \
         'readable', has_table_privilege(r.view, 'SELECT') \
         AND has_table_privilege(format('%I.%I', r.schema, r.name), 'SELECT')) \
         ORDER BY r.schema, r.name) \
         FROM {}.registry r \
         JOIN pg_class vc ON vc.oid = r.view \
         JOIN pg_namespace vn ON vn.oid = vc.relnamespace), '[]'::json))",
        ident(schema)
    )
}

/// Parse [`registry_sql`]'s reply.
///
/// # Errors
/// The reply is not the JSON object the query prints.
pub fn parse_registry(stdout: &str) -> Result<Registry, String> {
    serde_json::from_str(stdout.trim())
        .map_err(|error| format!("the pg_tviews registry query did not print its JSON: {error}"))
}

/// The SQL asking, for each readable TVIEW in `tviews`, whether it and its
/// backing view have rows: a JSON array of `[tview_has_rows, view_has_rows]`.
#[must_use]
pub fn emptiness_sql(tviews: &[&Tview]) -> String {
    let pairs: Vec<String> = tviews
        .iter()
        .map(|t| {
            format!(
                "json_build_array(EXISTS (SELECT 1 FROM {}.{}), EXISTS (SELECT 1 FROM {}.{}))",
                ident(&t.schema),
                ident(&t.name),
                ident(&t.view_schema),
                ident(&t.view_name)
            )
        })
        .collect();
    format!("SELECT json_build_array({})", pairs.join(", "))
}

/// Combine the registry with [`emptiness_sql`]'s reply for its readable TVIEWs.
///
/// # Errors
/// The reply is not one `[bool, bool]` pair per readable TVIEW.
pub fn assess_emptiness(registry: &Registry, stdout: &str) -> Result<Emptiness, String> {
    let pairs: Vec<(bool, bool)> = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("the TVIEW emptiness query did not print its JSON: {error}"))?;
    let readable: Vec<&Tview> = registry.tviews.iter().filter(|t| t.readable).collect();
    if pairs.len() != readable.len() {
        return Err(format!(
            "the TVIEW emptiness query answered for {} TVIEW(s), not {}",
            pairs.len(),
            readable.len()
        ));
    }
    let mut found = Emptiness::default();
    for (tview, (has_rows, view_has_rows)) in readable.into_iter().zip(pairs) {
        if !has_rows && view_has_rows {
            found.empty.push(tview.clone());
        }
    }
    for tview in registry.tviews.iter().filter(|t| !t.readable) {
        found
            .unreadable
            .push((tview.clone(), read_grant(tview, &registry.user)));
    }
    Ok(found)
}

/// The GRANT that lets `user` read `tview` (the backing view follows its
/// table's grants).
#[must_use]
pub fn read_grant(tview: &Tview, user: &str) -> String {
    format!(
        "GRANT SELECT ON {}.{} TO {}",
        ident(&tview.schema),
        ident(&tview.name),
        ident(user)
    )
}

/// The SQL reporting [`Identity`].
pub const IDENTITY_SQL: &str = "\
SELECT json_build_object('user', current_user, \
'superuser', (SELECT rolsuper FROM pg_roles WHERE rolname = current_user), \
'recovery', pg_is_in_recovery(), \
'system_identifier', (SELECT system_identifier::text FROM pg_control_system()))";

/// Parse [`IDENTITY_SQL`]'s reply.
///
/// # Errors
/// The reply is not the JSON object the query prints.
pub fn parse_identity(stdout: &str) -> Result<Identity, String> {
    serde_json::from_str(stdout.trim())
        .map_err(|error| format!("the identity query did not print its JSON: {error}"))
}

/// The SQL asking whether the connecting role may run `schema`'s
/// `pg_tviews_rebuild_all`, and who it is.
#[must_use]
pub fn rebuild_privilege_sql(schema: &str) -> String {
    format!(
        "SELECT json_build_object('user', current_user, 'execute', \
         has_function_privilege('{}.pg_tviews_rebuild_all(boolean)', 'EXECUTE'))",
        ident(schema).replace('\'', "''")
    )
}

/// Why the role cannot run the rebuild, with the GRANT that fixes it, from
/// [`rebuild_privilege_sql`]'s reply; `None` when it can.
///
/// # Errors
/// The reply is not the JSON object the query prints.
pub fn rebuild_privilege_problem(schema: &str, stdout: &str) -> Result<Option<String>, String> {
    #[derive(Deserialize)]
    struct Reply {
        user: String,
        execute: bool,
    }
    let reply: Reply = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("the rebuild-privilege query did not print its JSON: {error}"))?;
    if reply.execute {
        return Ok(None);
    }
    let function = format!("{}.pg_tviews_rebuild_all(boolean)", ident(schema));
    Ok(Some(format!(
        "role '{}' cannot execute {function}, which the TVIEW rebuild runs (pg_tviews \
         no longer grants it to PUBLIC). Fix: GRANT EXECUTE ON FUNCTION {function} TO {}",
        reply.user,
        ident(&reply.user)
    )))
}

/// The SQL running `schema`'s `pg_tviews_rebuild_all(only_empty => …)`,
/// printing its rows as a JSON array.
#[must_use]
pub fn rebuild_sql(schema: &str, only_empty: bool) -> String {
    format!(
        "SELECT COALESCE(json_agg(r), '[]'::json) \
         FROM {}.pg_tviews_rebuild_all(only_empty => {only_empty}) r",
        ident(schema)
    )
}

/// Parse [`rebuild_sql`]'s reply.
///
/// # Errors
/// The reply is not a JSON array of rows carrying `entity` and `rows_inserted`
/// (or `rows`).
pub fn parse_rebuild(stdout: &str) -> Result<Vec<Rebuilt>, String> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("pg_tviews_rebuild_all's reply is not JSON: {error}"))?;
    rows.iter()
        .map(|row| {
            let entity = row["entity"].as_str();
            let count = row
                .get("rows_inserted")
                .or_else(|| row.get("rows"))
                .and_then(serde_json::Value::as_i64);
            match (entity, count) {
                (Some(entity), Some(rows)) => Ok(Rebuilt {
                    entity: entity.to_owned(),
                    rows,
                }),
                _ => Err(format!(
                    "pg_tviews_rebuild_all returned {row}; fraisier reads entity and \
                     rows_inserted (or rows)"
                )),
            }
        })
        .collect()
}

/// Whether a `pg_restore --list` table of contents creates pg_tviews: restoring
/// it needs a superuser (`CREATE EXTENSION` of a C-language extension).
#[must_use]
pub fn archive_creates_pg_tviews(listing: &str) -> bool {
    listing.lines().any(|line| {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        // "<id>; <oid> <oid> EXTENSION - pg_tviews": the entry's type is the
        // fourth token, so a COMMENT on the extension does not count.
        tokens.get(3..6) == Some(&["EXTENSION", "-", "pg_tviews"][..])
    })
}

/// Run `sql` and return its bare output, or the first line of psql's error.
///
/// # Errors
/// psql could not run, or the query failed.
pub async fn query(psql: &Path, conn: &PgConn, sql: &str) -> Result<String, String> {
    let outcome = crate::run(query_command(psql, conn, sql))
        .await
        .map_err(|error| error.to_string())?;
    if outcome.succeeded() {
        Ok(outcome.stdout)
    } else {
        Err(outcome
            .stderr
            .trim()
            .lines()
            .next()
            .unwrap_or("psql failed")
            .to_owned())
    }
}

/// Every TVIEW in `schema` that is empty while its backing view has rows, as
/// the role behind `conn` sees them.
///
/// # Errors
/// A query failed or its reply was not understood.
pub async fn find_empty(psql: &Path, conn: &PgConn, schema: &str) -> Result<Emptiness, String> {
    let registry = parse_registry(&query(psql, conn, &registry_sql(schema)).await?)?;
    let readable: Vec<&Tview> = registry.tviews.iter().filter(|t| t.readable).collect();
    if readable.is_empty() {
        return assess_emptiness(&registry, "[]");
    }
    let reply = query(psql, conn, &emptiness_sql(&readable)).await?;
    assess_emptiness(&registry, &reply)
}

#[cfg(test)]
mod tests {
    use super::{
        archive_creates_pg_tviews, assess_emptiness, emptiness_sql, parse_identity, parse_rebuild,
        parse_registry, read_grant, rebuild_privilege_problem, rebuild_privilege_sql, rebuild_sql,
        registry_sql, Rebuilt, Registry, Tview,
    };

    fn tview(name: &str, readable: bool) -> Tview {
        Tview {
            schema: "public".to_owned(),
            name: name.to_owned(),
            view_schema: "tviews".to_owned(),
            view_name: format!("public__{name}"),
            readable,
        }
    }

    #[test]
    fn the_registry_query_reads_the_extension_schema_and_the_view_oid() {
        let sql = registry_sql("tviews");
        assert!(sql.contains("FROM \"tviews\".registry r"), "{sql}");
        assert!(sql.contains("vc.oid = r.view"), "{sql}");
        assert!(sql.contains("has_table_privilege"), "{sql}");
        // A schema name is quoted, never spliced bare.
        assert!(registry_sql("we\"ird").contains("\"we\"\"ird\".registry"));
    }

    #[test]
    fn a_registry_reply_parses() {
        let reply = r#"{"user" : "app", "tviews" : [{"schema" : "public", "name" : "tv_item", "view_schema" : "tviews", "view_name" : "public__tv_item", "readable" : true}]}"#;
        assert_eq!(
            parse_registry(reply).expect("parses"),
            Registry {
                user: "app".to_owned(),
                tviews: vec![tview("tv_item", true)],
            }
        );
    }

    #[test]
    fn a_registry_reply_that_is_not_json_is_an_error() {
        assert!(parse_registry("ERROR").is_err());
    }

    #[test]
    fn the_emptiness_query_asks_both_relations_per_tview() {
        let a = tview("tv_a", true);
        let b = tview("tv_b", true);
        let sql = emptiness_sql(&[&a, &b]);
        assert!(
            sql.contains("EXISTS (SELECT 1 FROM \"public\".\"tv_a\")"),
            "{sql}"
        );
        assert!(
            sql.contains("EXISTS (SELECT 1 FROM \"tviews\".\"public__tv_b\")"),
            "{sql}"
        );
    }

    #[test]
    fn an_empty_tview_over_a_populated_view_is_reported() {
        let registry = Registry {
            user: "app".to_owned(),
            tviews: vec![
                tview("tv_a", true),
                tview("tv_b", true),
                tview("tv_c", true),
            ],
        };
        // tv_a fine, tv_b empty over rows, tv_c empty over an empty view.
        let found =
            assess_emptiness(&registry, "[[true,true],[false,true],[false,false]]").expect("ok");
        assert_eq!(found.empty, vec![tview("tv_b", true)]);
        assert_eq!(found.unreadable, Vec::new());
    }

    #[test]
    fn an_unreadable_tview_is_named_with_its_grant_and_not_queried() {
        let registry = Registry {
            user: "ro".to_owned(),
            tviews: vec![tview("tv_a", false), tview("tv_b", true)],
        };
        let found = assess_emptiness(&registry, "[[false,true]]").expect("ok");
        assert_eq!(found.empty, vec![tview("tv_b", true)]);
        assert_eq!(
            found.unreadable,
            vec![(
                tview("tv_a", false),
                "GRANT SELECT ON \"public\".\"tv_a\" TO \"ro\"".to_owned()
            )]
        );
    }

    #[test]
    fn a_reply_with_the_wrong_number_of_pairs_is_an_error() {
        let registry = Registry {
            user: "app".to_owned(),
            tviews: vec![tview("tv_a", true)],
        };
        assert!(assess_emptiness(&registry, "[]").is_err());
        assert!(assess_emptiness(&registry, "[[true]]").is_err());
    }

    #[test]
    fn the_read_grant_quotes_both_names() {
        assert_eq!(
            read_grant(&tview("tv_a", false), "app\"x"),
            "GRANT SELECT ON \"public\".\"tv_a\" TO \"app\"\"x\""
        );
    }

    #[test]
    fn an_identity_reply_parses() {
        let identity = parse_identity(
            r#"{"user" : "postgres", "superuser" : true, "recovery" : false, "system_identifier" : "7700958424225614334"}"#,
        )
        .expect("parses");
        assert!(identity.superuser);
        assert_eq!(identity.system_identifier, "7700958424225614334");
    }

    #[test]
    fn a_role_without_execute_is_told_the_exact_grant() {
        assert!(rebuild_privilege_sql("tviews").contains("has_function_privilege"));
        let problem = rebuild_privilege_problem("tviews", r#"{"user" : "app", "execute" : false}"#)
            .expect("parses")
            .expect("a problem");
        assert!(
            problem.contains(
                "GRANT EXECUTE ON FUNCTION \"tviews\".pg_tviews_rebuild_all(boolean) TO \"app\""
            ),
            "{problem}"
        );
        assert_eq!(
            rebuild_privilege_problem("tviews", r#"{"user" : "app", "execute" : true}"#),
            Ok(None)
        );
    }

    #[test]
    fn the_rebuild_query_names_its_mode() {
        assert!(rebuild_sql("tviews", true).contains("pg_tviews_rebuild_all(only_empty => true)"));
        assert!(rebuild_sql("tviews", false).contains("only_empty => false"));
    }

    #[test]
    fn a_rebuild_reply_parses_either_count_column() {
        // rows_inserted from beta.29 on (captured on beta.32); rows before.
        assert_eq!(
            parse_rebuild(r#"[{"entity":"item","rows_inserted":2}]"#),
            Ok(vec![Rebuilt {
                entity: "item".to_owned(),
                rows: 2
            }])
        );
        assert_eq!(
            parse_rebuild(r#"[{"entity":"item","rows":5}]"#).expect("parses")[0].rows,
            5
        );
        assert_eq!(parse_rebuild("[]"), Ok(Vec::new()));
        assert!(parse_rebuild(r#"[{"entity":"item"}]"#).is_err());
    }

    /// `pg_restore --list` of a pg_tviews database (captured on PG 18.6,
    /// pg_tviews 0.1.0-beta.32).
    const LISTING: &str = "\
;
; Archive created at 2026-10-26 13:20:00 UTC
;
2; 3079 16390 EXTENSION - pg_tviews
4158; 0 0 COMMENT - EXTENSION pg_tviews
232; 1259 16555 TABLE public tv_item app
231; 1259 16551 VIEW tviews public__tv_item app
4150; 0 16555 TABLE DATA public tv_item app
";

    #[test]
    fn an_archive_that_creates_pg_tviews_is_recognised() {
        assert!(archive_creates_pg_tviews(LISTING));
    }

    #[test]
    fn a_comment_or_a_table_name_alone_is_not_the_extension() {
        assert!(!archive_creates_pg_tviews(
            "4158; 0 0 COMMENT - EXTENSION pg_tviews \n232; 1259 16555 TABLE public pg_tviews app\n"
        ));
        assert!(!archive_creates_pg_tviews(
            "2; 3079 16390 EXTENSION - pgcrypto \n"
        ));
    }
}

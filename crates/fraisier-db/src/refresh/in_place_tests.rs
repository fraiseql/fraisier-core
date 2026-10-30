use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;

use super::{refresh_in_place, MarkerServer, RefreshServer, SchemaPrivilege};
use crate::restore::{Admin, RestoreError, RestoreIdentity, RestoreServer, Trigger};
use crate::tviews::{Identity, Rebuilt};
use crate::PgConn;

const MARKER: &str = "checkout/staging";

/// A plain archive: one table and its rows.
const PLAIN: &str = "220; 1259 16400 TABLE public tb_item app_prod\n\
                     221; 0 16400 TABLE DATA public tb_item app_prod\n";

/// A pg_tviews archive, as measured on PostgreSQL 18 + pg_tviews beta.32.
const TVIEWS: &str = "7; 2615 16543 SCHEMA - tviews postgres\n\
                      4157; 0 0 ACL - SCHEMA tviews postgres\n\
                      2; 3079 16544 EXTENSION - pg_tviews \n\
                      4158; 0 0 COMMENT - EXTENSION pg_tviews \n\
                      230; 1259 16693 TABLE public tb_item app_prod\n\
                      231; 1259 16704 VIEW tviews public__tv_item app_prod\n\
                      4149; 0 16693 TABLE DATA public tb_item app_prod\n\
                      3950; 0 16566 TABLE DATA tviews pg_tview_meta postgres\n";

fn app() -> PgConn {
    PgConn::parse("postgres://app:s3cret@db.internal/checkout_staging").expect("dsn")
}

fn identity(admin: bool) -> RestoreIdentity {
    let admin = admin.then(|| Admin {
        env: "CHECKOUT_ADMIN_URL".to_owned(),
        conn: PgConn::parse("postgres://postgres@%2Frun%2Fpostgresql/postgres").expect("dsn"),
    });
    RestoreIdentity::new(app(), admin, Trigger::Operator)
}

/// A server that answers from fixed values and records each call as
/// `op:user@database[ detail]`.
struct Fake {
    listing: Option<&'static str>,
    marker: Option<&'static str>,
    marker_privilege: Option<&'static str>,
    connections: Vec<String>,
    extensions_problem: Option<&'static str>,
    first_pass_fails: bool,
    still_empty: Vec<String>,
    calls: Mutex<Vec<String>>,
}

impl Fake {
    fn new(listing: &'static str) -> Self {
        Self {
            listing: Some(listing),
            marker: Some(MARKER),
            marker_privilege: None,
            connections: Vec::new(),
            extensions_problem: None,
            first_pass_fails: false,
            still_empty: Vec::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn record(&self, op: &str, conn: &PgConn) {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("{op}:{}@{}", user(conn), conn.database()));
    }

    /// Whether anything that changes the target ran.
    fn changed(&self) -> bool {
        self.calls().iter().any(|call| {
            ["drop", "create", "set_marker", "restore", "grant", "revoke"]
                .iter()
                .any(|op| call.starts_with(op))
        })
    }

    /// The position of the first call starting with `op`.
    fn at(&self, op: &str) -> usize {
        self.calls()
            .iter()
            .position(|call| call.starts_with(op))
            .unwrap_or_else(|| panic!("no {op} in {:?}", self.calls()))
    }
}

fn user(conn: &PgConn) -> String {
    let dsn = conn.dsn();
    let rest = dsn.trim_start_matches("postgres://");
    rest.split(['@', ':']).next().unwrap_or_default().to_owned()
}

#[async_trait]
impl RestoreServer for Fake {
    async fn list(&self, archive: &Path) -> Result<String, String> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("list:{}", archive.display()));
        self.listing
            .map(str::to_owned)
            .ok_or_else(|| "pg_restore: input file does not appear to be a valid archive".into())
    }

    async fn identity(&self, conn: &PgConn) -> Result<Identity, String> {
        self.record("identity", conn);
        Ok(Identity {
            user: user(conn),
            superuser: user(conn) == "postgres",
            recovery: false,
            system_identifier: "7700958424225614334".to_owned(),
        })
    }

    async fn current_user(&self, conn: &PgConn) -> Result<String, String> {
        self.record("current_user", conn);
        Ok(user(conn))
    }

    async fn dump(&self, _conn: &PgConn, _out: &Path) -> Result<(), String> {
        unreachable!("a refresh never dumps")
    }

    async fn create(
        &self,
        maintenance: &PgConn,
        name: &str,
        owner: Option<&str>,
    ) -> Result<(), String> {
        self.record(
            &format!("create {name} owner {}", owner.unwrap_or("-")),
            maintenance,
        );
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

    async fn drop(&self, maintenance: &PgConn, name: &str) -> Result<(), String> {
        self.record(&format!("drop {name}"), maintenance);
        Ok(())
    }

    async fn rebuild(&self, conn: &PgConn, only_empty: bool) -> Result<Vec<Rebuilt>, String> {
        self.record(&format!("rebuild only_empty={only_empty}"), conn);
        Ok(vec![Rebuilt {
            entity: "item".to_owned(),
            rows: 2,
        }])
    }

    async fn still_empty(&self, conn: &PgConn) -> Result<Vec<String>, String> {
        self.record("still_empty", conn);
        Ok(self.still_empty.clone())
    }
}

#[async_trait]
impl MarkerServer for Fake {
    async fn marker(&self, conn: &PgConn) -> Result<Option<String>, String> {
        self.record("marker", conn);
        Ok(self.marker.map(str::to_owned))
    }

    async fn marker_privilege(&self, conn: &PgConn) -> Result<Option<String>, String> {
        self.record("marker_privilege", conn);
        Ok(self.marker_privilege.map(str::to_owned))
    }

    async fn set_marker(&self, conn: &PgConn, value: &str) -> Result<(), String> {
        self.record(&format!("set_marker {value}"), conn);
        Ok(())
    }
}

#[async_trait]
impl RefreshServer for Fake {
    async fn connections(&self, maintenance: &PgConn, db: &str) -> Result<Vec<String>, String> {
        self.record(&format!("connections {db}"), maintenance);
        Ok(self.connections.clone())
    }

    async fn extensions_problem(
        &self,
        conn: &PgConn,
        extensions: &[String],
    ) -> Result<Option<String>, String> {
        self.record(
            &format!("extensions_problem {}", extensions.join(",")),
            conn,
        );
        Ok(self.extensions_problem.map(str::to_owned))
    }

    async fn database_bytes(&self, conn: &PgConn) -> Result<u64, String> {
        self.record("database_bytes", conn);
        Ok(0)
    }

    async fn table_count(&self, conn: &PgConn) -> Result<u64, String> {
        self.record("table_count", conn);
        Ok(0)
    }

    async fn create_extensions(&self, conn: &PgConn, extensions: &[String]) -> Result<(), String> {
        self.record(&format!("create_extensions {}", extensions.join(",")), conn);
        Ok(())
    }

    async fn schemas(&self, conn: &PgConn) -> Result<Vec<String>, String> {
        self.record("schemas", conn);
        Ok(vec!["public".to_owned(), "tviews".to_owned()])
    }

    async fn schema_privilege(
        &self,
        conn: &PgConn,
        privilege: SchemaPrivilege,
        schema: &str,
        role: &str,
        revoke: bool,
    ) -> Result<(), String> {
        let verb = if revoke { "revoke" } else { "grant" };
        self.record(&format!("{verb} {privilege:?} {schema} {role}"), conn);
        Ok(())
    }

    async fn restore_list(
        &self,
        conn: &PgConn,
        _archive: &Path,
        list: &str,
        role: Option<&str>,
        data_only: bool,
    ) -> Result<String, String> {
        let pass = if data_only { "data" } else { "objects" };
        self.record(
            &format!(
                "restore {pass} as {} ({} entries)",
                role.unwrap_or("-"),
                list.lines().count()
            ),
            conn,
        );
        if self.first_pass_fails && !data_only {
            return Err("pg_restore: error: could not execute query".to_owned());
        }
        Ok(String::new())
    }
}

async fn refresh(fake: &Fake, admin: bool) -> Result<super::Refreshed, RestoreError> {
    refresh_in_place(fake, &identity(admin), Path::new("/b/a.pgdump"), MARKER).await
}

fn refused(result: Result<super::Refreshed, RestoreError>) -> String {
    match result {
        Err(RestoreError::Refused(reason)) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unreadable_archive_is_refused_before_anything_changes() {
    let fake = Fake {
        listing: None,
        ..Fake::new(PLAIN)
    };
    let reason = refused(refresh(&fake, false).await);
    assert!(reason.contains("pg_restore --list"), "{reason}");
    assert!(!fake.changed(), "{:?}", fake.calls());
}

#[tokio::test]
async fn an_unmarked_target_is_refused_with_the_marking_command() {
    let fake = Fake {
        marker: None,
        ..Fake::new(PLAIN)
    };
    let reason = refused(refresh(&fake, false).await);
    assert!(reason.contains("mark-refresh-target"), "{reason}");
    assert!(!fake.changed(), "{:?}", fake.calls());
}

#[tokio::test]
async fn a_target_marked_for_another_config_is_refused() {
    let fake = Fake {
        marker: Some("checkout/qa"),
        ..Fake::new(PLAIN)
    };
    let reason = refused(refresh(&fake, false).await);
    assert!(reason.contains("checkout/qa"), "{reason}");
    assert!(!fake.changed(), "{:?}", fake.calls());
}

#[tokio::test]
async fn a_restorer_that_could_not_mark_again_is_refused_with_the_grant() {
    let fake = Fake {
        marker_privilege: Some("GRANT SET ON PARAMETER fraisier.refresh_target TO \"app\""),
        ..Fake::new(PLAIN)
    };
    let reason = refused(refresh(&fake, false).await);
    assert!(reason.contains("GRANT SET ON PARAMETER"), "{reason}");
    assert!(!fake.changed(), "{:?}", fake.calls());
}

#[tokio::test]
async fn a_session_still_connected_to_the_target_is_refused_by_name() {
    let fake = Fake {
        connections: vec!["app (checkout-worker, pid 4242)".to_owned()],
        ..Fake::new(PLAIN)
    };
    let reason = refused(refresh(&fake, false).await);
    assert!(reason.contains("checkout-worker, pid 4242"), "{reason}");
    assert!(!fake.changed(), "{:?}", fake.calls());
}

#[tokio::test]
async fn an_extension_the_restorer_cannot_create_is_refused() {
    let fake = Fake {
        extensions_problem: Some("the server has no extension postgis"),
        ..Fake::new("2; 3079 16544 EXTENSION - postgis \n220; 1259 16400 TABLE public t app_prod\n")
    };
    let reason = refused(refresh(&fake, false).await);
    assert!(reason.contains("postgis"), "{reason}");
    assert!(!fake.changed(), "{:?}", fake.calls());
}

#[tokio::test]
async fn a_pg_tviews_archive_is_refused_without_the_admin_before_the_drop() {
    let fake = Fake::new(TVIEWS);
    let reason = refused(refresh(&fake, false).await);
    assert!(reason.contains("superuser"), "{reason}");
    assert!(reason.contains("admin_url_env"), "{reason}");
    assert!(!fake.changed(), "{:?}", fake.calls());
}

#[tokio::test]
async fn the_marker_and_sessions_are_checked_on_the_target_right_before_the_drop() {
    let fake = Fake::new(PLAIN);
    refresh(&fake, false).await.expect("refreshed");
    let calls = fake.calls();
    let drop = fake.at("drop");
    // The last two reads before the drop are of the database being dropped.
    assert_eq!(calls[drop - 2], "marker:app@checkout_staging", "{calls:?}");
    assert_eq!(
        calls[drop - 1],
        "connections checkout_staging:app@postgres",
        "{calls:?}"
    );
}

#[tokio::test]
async fn a_plain_archive_is_refreshed_by_the_app_role_alone() {
    let fake = Fake::new(PLAIN);
    let refreshed = refresh(&fake, false).await.expect("refreshed");
    assert_eq!(refreshed.tviews, None);
    let after_drop: Vec<String> = fake.calls()[fake.at("drop")..].to_vec();
    assert_eq!(
        after_drop,
        vec![
            "drop checkout_staging:app@postgres",
            "create checkout_staging owner -:app@postgres",
            "set_marker checkout/staging:app@checkout_staging",
            "schemas:app@checkout_staging",
            "restore objects as - (2 entries):app@checkout_staging",
        ]
    );
}

#[tokio::test]
async fn a_pg_tviews_archive_is_refreshed_in_two_passes_and_owned_by_the_app() {
    let fake = Fake::new(TVIEWS);
    let refreshed = refresh(&fake, true).await.expect("refreshed");
    assert_eq!(refreshed.extensions, vec!["pg_tviews"]);
    assert!(refreshed.tviews.is_some());
    let after_drop: Vec<String> = fake.calls()[fake.at("drop")..].to_vec();
    assert_eq!(
        after_drop,
        vec![
            "drop checkout_staging:postgres@postgres",
            "create checkout_staging owner app:postgres@postgres",
            "set_marker checkout/staging:postgres@checkout_staging",
            "create_extensions pg_tviews:postgres@checkout_staging",
            "schemas:postgres@checkout_staging",
            "grant Usage tviews app:postgres@checkout_staging",
            "grant Create tviews app:postgres@checkout_staging",
            "restore objects as app (3 entries):postgres@checkout_staging",
            "revoke Create tviews app:postgres@checkout_staging",
            "restore data as - (1 entries):postgres@checkout_staging",
            "rebuild only_empty=true:postgres@checkout_staging",
            "still_empty:postgres@checkout_staging",
        ]
    );
}

#[tokio::test]
async fn a_failed_first_pass_still_revokes_the_temporary_grant() {
    let fake = Fake {
        first_pass_fails: true,
        ..Fake::new(TVIEWS)
    };
    match refresh(&fake, true).await {
        Err(RestoreError::Failed(reason)) => assert!(reason.contains("pg_restore"), "{reason}"),
        other => panic!("expected a failure, got {other:?}"),
    }
    assert!(fake
        .calls()
        .contains(&"revoke Create tviews app:postgres@checkout_staging".to_owned()));
    assert!(!fake.calls().iter().any(|c| c.starts_with("restore data")));
}

#[tokio::test]
async fn a_tview_still_empty_after_the_rebuild_fails_the_refresh() {
    let fake = Fake {
        still_empty: vec!["public.tv_item".to_owned()],
        ..Fake::new(TVIEWS)
    };
    match refresh(&fake, true).await {
        Err(RestoreError::Failed(reason)) => assert!(reason.contains("public.tv_item"), "{reason}"),
        other => panic!("expected a failure, got {other:?}"),
    }
}

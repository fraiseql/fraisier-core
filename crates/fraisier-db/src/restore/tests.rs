use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;

use super::{
    check_room, drop_throwaway, restore_in_place, restore_throwaway, room_for_copy, teardown_guard,
    throwaway_name, Admin, RestoreError, RestoreIdentity, RestoreServer, Seed, Trigger,
    THROWAWAY_MARKER,
};
use crate::tviews::{Identity, Rebuilt};
use crate::PgConn;

/// `pg_restore --list` of an archive that creates pg_tviews.
const TVIEWS_LISTING: &str = "2; 3079 16390 EXTENSION - pg_tviews\n\
                              232; 1259 16555 TABLE public tb_item app\n";
/// `pg_restore --list` of a plain archive.
const PLAIN_LISTING: &str = "232; 1259 16555 TABLE public tb_item app\n";

fn app() -> PgConn {
    PgConn::parse("postgres://app:s3cret@db.internal/shop").expect("dsn")
}

fn admin() -> Admin {
    Admin {
        env: "SHOP_ADMIN_URL".to_owned(),
        conn: PgConn::parse("postgres://postgres@%2Frun%2Fpostgresql/postgres").expect("dsn"),
    }
}

/// A server that records each call as `op:user@database[:detail]`.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    listing: Option<&'static str>,
    restore_fails: bool,
    rebuild_fails: bool,
    still_empty: Vec<String>,
}

impl Fake {
    fn listing(listing: &'static str) -> Self {
        Self {
            listing: Some(listing),
            ..Self::default()
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
}

/// The user a connection would log in as, from its rendered DSN.
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

    async fn dump(&self, conn: &PgConn, _out: &Path) -> Result<(), String> {
        self.record("dump", conn);
        Ok(())
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

    async fn restore(&self, conn: &PgConn, _archive: &Path, clean: bool) -> Result<String, String> {
        self.record(if clean { "restore --clean" } else { "restore" }, conn);
        if self.restore_fails {
            Err("pg_restore: error: could not execute query".to_owned())
        } else {
            Ok(String::new())
        }
    }

    async fn drop(&self, maintenance: &PgConn, name: &str) -> Result<(), String> {
        self.record(&format!("drop {name}"), maintenance);
        Ok(())
    }

    async fn rebuild(&self, conn: &PgConn, only_empty: bool) -> Result<Vec<Rebuilt>, String> {
        self.record(&format!("rebuild only_empty={only_empty}"), conn);
        if self.rebuild_fails {
            return Err("role 'app' cannot execute pg_tviews_rebuild_all".to_owned());
        }
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

fn operator(admin: Option<Admin>) -> RestoreIdentity {
    RestoreIdentity::new(app(), admin, Trigger::Operator)
}

fn archive() -> &'static Path {
    Path::new("/backups/shop.pgdump")
}

/// The calls, with the generated throwaway name replaced by `T`.
fn named(fake: &Fake, name: &str) -> Vec<String> {
    fake.calls()
        .into_iter()
        .map(|call| call.replace(name, "T"))
        .collect()
}

// --- the identity ---

#[test]
fn without_an_admin_the_app_role_restores() {
    let identity = operator(None);
    assert!(!identity.uses_admin());
    assert_eq!(user(&identity.restorer("shop")), "app");
}

#[test]
fn an_operator_with_an_admin_restores_as_the_admin() {
    let identity = operator(Some(admin()));
    assert!(identity.uses_admin());
    let restorer = identity.restorer("shop");
    assert_eq!(user(&restorer), "postgres");
    assert_eq!(restorer.database(), "shop");
}

#[test]
fn a_webhook_never_restores_as_the_admin() {
    let identity = RestoreIdentity::new(app(), Some(admin()), Trigger::Webhook);
    assert!(!identity.uses_admin());
    assert_eq!(user(&identity.restorer("shop")), "app");
}

#[test]
fn a_webhook_never_reads_the_admin_env_var() {
    // Unset on purpose: an operator would be refused, the webhook never looks.
    let env = "FRAISIER_RESTORE_TEST_ADMIN_NEVER_SET";
    let identity =
        RestoreIdentity::from_env(app(), Some(env), Trigger::Webhook).expect("never read");
    assert!(!identity.uses_admin());
    let refused = RestoreIdentity::from_env(app(), Some(env), Trigger::Operator)
        .expect_err("an operator's configured identity must resolve");
    assert!(refused.contains(env), "{refused}");
}

#[test]
fn no_admin_configured_needs_no_env_var() {
    let identity = RestoreIdentity::from_env(app(), None, Trigger::Operator).expect("app role");
    assert!(!identity.uses_admin());
}

// --- in place ---

#[tokio::test]
async fn an_unreadable_archive_is_refused_before_anything_runs() {
    let fake = Fake::default();
    let error = restore_in_place(&fake, &operator(None), archive())
        .await
        .expect_err("refused");
    assert!(
        matches!(&error, RestoreError::Refused(m) if m.contains("cannot read the archive")),
        "{error:?}"
    );
    assert_eq!(fake.calls(), ["list:/backups/shop.pgdump"]);
}

#[tokio::test]
async fn a_plain_archive_restores_in_place_with_no_tviews_step() {
    let fake = Fake::listing(PLAIN_LISTING);
    let restored = restore_in_place(&fake, &operator(None), archive())
        .await
        .expect("restored");
    assert_eq!(restored.tviews, None);
    assert_eq!(
        fake.calls(),
        ["list:/backups/shop.pgdump", "restore --clean:app@shop"]
    );
}

#[tokio::test]
async fn a_pg_tviews_archive_is_refused_for_the_app_role_before_the_drop() {
    let fake = Fake::listing(TVIEWS_LISTING);
    let error = restore_in_place(&fake, &operator(None), archive())
        .await
        .expect_err("refused");
    assert!(
        matches!(&error, RestoreError::Refused(m) if m.contains("only a superuser")),
        "{error:?}"
    );
    assert_eq!(
        fake.calls(),
        ["list:/backups/shop.pgdump", "identity:app@shop"],
        "nothing is dropped once the restorer is refused"
    );
}

#[tokio::test]
async fn a_pg_tviews_archive_restores_as_the_admin_then_rebuilds_and_probes() {
    let fake = Fake::listing(TVIEWS_LISTING);
    let restored = restore_in_place(&fake, &operator(Some(admin())), archive())
        .await
        .expect("restored");
    let rebuild = restored.tviews.expect("rebuilt").expect("ok");
    assert_eq!(rebuild.rebuilt[0].entity, "item");
    assert_eq!(rebuild.still_empty, Vec::<String>::new());
    assert_eq!(
        fake.calls(),
        [
            "list:/backups/shop.pgdump",
            "identity:postgres@shop",
            "restore --clean:postgres@shop",
            "rebuild only_empty=true:postgres@shop",
            "still_empty:postgres@shop",
        ]
    );
}

#[tokio::test]
async fn a_failed_rebuild_after_a_good_in_place_restore_is_reported_not_raised() {
    let fake = Fake {
        rebuild_fails: true,
        ..Fake::listing(TVIEWS_LISTING)
    };
    let restored = restore_in_place(&fake, &operator(Some(admin())), archive())
        .await
        .expect("the restore itself succeeded");
    let error = restored.tviews.expect("attempted").expect_err("failed");
    assert!(error.contains("cannot execute"), "{error}");
}

#[tokio::test]
async fn a_failed_in_place_restore_is_failed_not_refused() {
    let fake = Fake {
        restore_fails: true,
        ..Fake::listing(PLAIN_LISTING)
    };
    let error = restore_in_place(&fake, &operator(None), archive())
        .await
        .expect_err("failed");
    assert!(matches!(error, RestoreError::Failed(_)), "{error:?}");
}

// --- into a throwaway ---

#[tokio::test]
async fn a_throwaway_is_created_restored_and_left_for_its_caller() {
    let fake = Fake::listing(PLAIN_LISTING);
    let copy = restore_throwaway(&fake, &operator(None), Seed::Archive(archive()), true)
        .await
        .expect("restored");
    assert!(copy.name.contains(THROWAWAY_MARKER), "{}", copy.name);
    assert_eq!(copy.app.database(), copy.name);
    assert_eq!(user(&copy.app), "app");
    assert_eq!(copy.tviews, None);
    assert_eq!(
        named(&fake, &copy.name),
        [
            "list:/backups/shop.pgdump",
            "create T owner -:app@postgres",
            "restore:app@T",
        ]
    );
}

#[tokio::test]
async fn an_admin_creates_the_throwaway_for_the_app_role_and_restores_it() {
    let fake = Fake::listing(TVIEWS_LISTING);
    let copy = restore_throwaway(
        &fake,
        &operator(Some(admin())),
        Seed::Archive(archive()),
        true,
    )
    .await
    .expect("restored");
    assert_eq!(
        user(&copy.app),
        "app",
        "migrations still run as the app role"
    );
    assert_eq!(
        copy.tviews.expect("rebuilt").rebuilt[0].entity,
        "item".to_owned()
    );
    assert_eq!(
        named(&fake, &copy.name),
        [
            "list:/backups/shop.pgdump",
            "identity:postgres@shop",
            "current_user:app@shop",
            "create T owner app:postgres@postgres",
            "restore:postgres@T",
            "rebuild only_empty=true:postgres@T",
            "still_empty:postgres@T",
        ]
    );
}

#[tokio::test]
async fn a_pg_tviews_throwaway_is_refused_for_the_app_role_before_it_is_created() {
    let fake = Fake::listing(TVIEWS_LISTING);
    let error = restore_throwaway(&fake, &operator(None), Seed::Archive(archive()), true)
        .await
        .expect_err("refused");
    assert!(
        matches!(&error, RestoreError::Refused(m) if m.contains("only a superuser")
            && m.contains("[database].admin_url_env")),
        "{error:?}"
    );
    assert!(
        !fake.calls().iter().any(|c| c.starts_with("create")),
        "{:?}",
        fake.calls()
    );
}

#[tokio::test]
async fn a_webhook_restore_of_a_pg_tviews_archive_points_at_the_cli() {
    let fake = Fake::listing(TVIEWS_LISTING);
    let identity = RestoreIdentity::new(app(), Some(admin()), Trigger::Webhook);
    let error = restore_throwaway(&fake, &identity, Seed::Archive(archive()), false)
        .await
        .expect_err("refused");
    let RestoreError::Refused(message) = &error else {
        panic!("{error:?}");
    };
    assert!(message.contains("webhook"), "{message}");
    assert!(message.contains("CLI"), "{message}");
    assert!(message.contains("SHOP_ADMIN_URL"), "{message}");
    assert!(
        !fake.calls().iter().any(|c| c.contains("postgres@")),
        "the admin identity is never used: {:?}",
        fake.calls()
    );
}

#[tokio::test]
async fn without_rebuild_a_pg_tviews_throwaway_is_not_rebuilt() {
    let fake = Fake::listing(TVIEWS_LISTING);
    let copy = restore_throwaway(
        &fake,
        &operator(Some(admin())),
        Seed::Archive(archive()),
        false,
    )
    .await
    .expect("restored");
    assert_eq!(copy.tviews, None);
    assert!(!fake.calls().iter().any(|c| c.starts_with("rebuild")));
}

#[tokio::test]
async fn a_fresh_dump_is_taken_as_the_app_role_before_the_listing() {
    let fake = Fake::listing(PLAIN_LISTING);
    let copy = restore_throwaway(&fake, &operator(None), Seed::FreshDump, true)
        .await
        .expect("restored");
    let calls = named(&fake, &copy.name);
    assert_eq!(calls[0], "dump:app@shop");
    assert!(calls[1].starts_with("list:"), "{calls:?}");
}

#[tokio::test]
async fn a_throwaway_that_does_not_restore_is_dropped_again() {
    let fake = Fake {
        restore_fails: true,
        ..Fake::listing(PLAIN_LISTING)
    };
    let error = restore_throwaway(&fake, &operator(None), Seed::Archive(archive()), true)
        .await
        .expect_err("failed");
    assert!(
        matches!(&error, RestoreError::Failed(m) if m.contains("could not execute")),
        "{error:?}"
    );
    let calls = fake.calls();
    assert!(
        calls.last().is_some_and(|c| c.starts_with("drop ")),
        "{calls:?}"
    );
}

#[tokio::test]
async fn a_throwaway_whose_rebuild_fails_is_dropped_again() {
    let fake = Fake {
        rebuild_fails: true,
        ..Fake::listing(TVIEWS_LISTING)
    };
    let error = restore_throwaway(
        &fake,
        &operator(Some(admin())),
        Seed::Archive(archive()),
        true,
    )
    .await
    .expect_err("failed");
    assert!(
        matches!(&error, RestoreError::Failed(m) if m.contains("TVIEW rebuild")),
        "{error:?}"
    );
    assert!(fake.calls().last().is_some_and(|c| c.starts_with("drop ")));
}

#[tokio::test]
async fn a_throwaway_left_with_an_empty_tview_fails_and_is_dropped() {
    let fake = Fake {
        still_empty: vec!["public.tv_item".to_owned()],
        ..Fake::listing(TVIEWS_LISTING)
    };
    let error = restore_throwaway(
        &fake,
        &operator(Some(admin())),
        Seed::Archive(archive()),
        true,
    )
    .await
    .expect_err("failed");
    assert!(
        matches!(&error, RestoreError::Failed(m) if m.contains("public.tv_item")),
        "{error:?}"
    );
    assert!(fake.calls().last().is_some_and(|c| c.starts_with("drop ")));
}

// --- teardown ---

#[test]
fn the_guard_admits_a_marked_name_that_is_not_live() {
    assert_eq!(teardown_guard("shop_fraisier_rh_1_a", Some("shop")), Ok(()));
    assert_eq!(teardown_guard("shop_fraisier_rh_1_a", None), Ok(()));
}

#[test]
fn the_guard_refuses_a_name_without_the_marker() {
    let reason = teardown_guard("shop_copy", None).expect_err("no marker");
    assert!(reason.contains(THROWAWAY_MARKER), "{reason}");
}

#[test]
fn the_guard_refuses_the_live_database_even_with_the_marker() {
    let reason =
        teardown_guard("shop_fraisier_rh_x", Some("shop_fraisier_rh_x")).expect_err("live");
    assert!(reason.contains("live"), "{reason}");
}

#[tokio::test]
async fn a_guarded_drop_runs_nothing() {
    let fake = Fake::default();
    let refused = drop_throwaway(&fake, &operator(None), "shop", None)
        .await
        .expect_err("refused");
    assert!(refused.contains(THROWAWAY_MARKER), "{refused}");
    assert_eq!(fake.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn a_throwaway_is_dropped_as_the_identity_that_created_it() {
    let fake = Fake::default();
    drop_throwaway(
        &fake,
        &operator(Some(admin())),
        "shop_fraisier_rh_1_a",
        Some("shop"),
    )
    .await
    .expect("dropped");
    assert_eq!(
        fake.calls(),
        ["drop shop_fraisier_rh_1_a:postgres@postgres"]
    );
}

// --- free space and names ---

#[test]
fn a_copy_needs_the_live_size_plus_headroom() {
    let dir = Path::new("/var/lib/postgresql");
    assert_eq!(check_room(1_000_000_000, 3_000_000_000, dir), Ok(()));
    let refused = check_room(1_000_000_000, 2_999_999_999, dir).expect_err("too little");
    assert!(refused.contains("/var/lib/postgresql"), "{refused}");
    assert!(refused.contains("3 GB"), "{refused}");
}

#[test]
fn a_copy_that_cannot_fit_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let error = room_for_copy(u64::MAX / 2, dir.path()).expect_err("no filesystem holds that");
    assert!(error.contains("GB free"), "{error}");
}

#[test]
fn a_free_space_that_cannot_be_measured_refuses() {
    let error = room_for_copy(0, Path::new("/nonexistent/fraisier"))
        .expect_err("an unmeasurable volume is not a roomy one");
    assert!(error.contains("could not measure"), "{error}");
}

#[test]
fn a_small_copy_fits_in_the_temp_dir() {
    room_for_copy(0, &std::env::temp_dir()).expect("2 GB free in the temp dir");
}

#[test]
fn throwaway_name_is_a_valid_bounded_identifier() {
    for base in [
        "app",
        "shop_db",
        "9starts_with_digit",
        "",
        "weird-name.with.dots",
    ] {
        let name = throwaway_name(base);
        assert!(name.len() <= 63, "name {name:?} exceeds 63 chars");
        assert!(name.contains(THROWAWAY_MARKER), "{name:?}");
        assert!(
            name.starts_with(|c: char| c.is_ascii_alphabetic()),
            "name {name:?} must start with a letter"
        );
        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "name {name:?} has an invalid character"
        );
    }
}

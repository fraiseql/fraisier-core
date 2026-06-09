//! Integration tests for the in-process Confiture adapter.
//!
//! These exercise the *real* `confiture` binary. They skip gracefully when it is
//! absent so the suite stays green on machines without Confiture installed. The
//! full Postgres round-trip runs only when `FRAISIER_TEST_DATABASE_URL` points at
//! a usable, empty database.

use fraisier_adapter_confiture::ConfitureMigration;
use fraisier_core::adapter_axes::{AdapterCtx, MigrationAdapter, Revision};
use tokio::sync::Mutex;

/// `set_var`/`var` are process-global; serialise the env-mutating tests. An
/// async mutex so the guard may be held across the `confiture` spawn (the env var
/// must stay set while the child reads it).
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// Whether a working `confiture` is reachable (so we can skip when it is not).
async fn confiture_available() -> bool {
    ConfitureMigration::new().describe().await.is_ok()
}

#[tokio::test]
async fn describe_reports_version_and_capabilities() {
    if !confiture_available().await {
        eprintln!("skipping: confiture not on PATH");
        return;
    }
    let desc = ConfitureMigration::new()
        .describe()
        .await
        .expect("describe succeeds when confiture is present");

    assert_eq!(desc.name, "confiture");
    assert_eq!(desc.protocol_version, 1);
    assert!(!desc.version.is_empty());
    // The forward-compat lint capability must be advertised so the deploy layer
    // knows it can call preflight.
    assert!(
        desc.capabilities.iter().any(|cap| cap == "preflight"),
        "capabilities = {:?}",
        desc.capabilities
    );
    // post_migrate is intentionally NOT advertised (trait no-op).
    assert!(!desc.capabilities.iter().any(|cap| cap == "post_migrate"));
}

/// The load-bearing safety test: an injected `CONFITURE_DATABASE_URL` must win
/// over a decoy `db/environments/local.yaml` sitting in the workdir — otherwise a
/// production deploy could silently migrate the wrong database.
#[tokio::test]
async fn env_dsn_beats_decoy_config() {
    if !confiture_available().await {
        eprintln!("skipping: confiture not on PATH");
        return;
    }
    let _guard = ENV_LOCK.lock().await;

    // A throwaway project dir containing a decoy config pointing at port 2.
    let dir = std::env::temp_dir().join(format!("fraisier-decoy-{}", std::process::id()));
    let envs = dir.join("db/environments");
    std::fs::create_dir_all(dir.join("db/migrations")).expect("mkdir migrations");
    std::fs::create_dir_all(&envs).expect("mkdir environments");
    std::fs::write(
        envs.join("local.yaml"),
        "name: local\ndatabase_url: postgresql://decoy@127.0.0.1:2/configdb\n\
         migration:\n  tracking_table: confiture_migrations\n",
    )
    .expect("write decoy config");

    // The injected DSN points at port 1 (also unreachable). If the adapter is
    // correct, confiture dials port 1 (env) and never port 2 (decoy config).
    let source = "FRAISIER_CONF_IT_DECOY_DSN";
    std::env::set_var(source, "postgresql://injected@127.0.0.1:1/envdb");

    let mut ctx = AdapterCtx::new("checkout", "production");
    ctx.workdir = dir.clone();
    ctx.migrations_path = Some(dir.join("db/migrations"));
    ctx.env_secrets
        .insert("DATABASE_URL".to_owned(), source.to_owned());

    let err = ConfitureMigration::new()
        .current_revision(&ctx)
        .await
        .expect_err("unreachable DSN must surface a connection error");

    std::env::remove_var(source);
    let _ = std::fs::remove_dir_all(&dir);

    let rendered = format!("{}{}", err, err.stderr.as_deref().unwrap_or(""));
    assert!(
        rendered.contains("port 1"),
        "expected the injected env DSN (port 1) to be used; got: {rendered}"
    );
    assert!(
        !rendered.contains("port 2"),
        "decoy config DSN (port 2) must never be dialed; got: {rendered}"
    );
}

/// Full round-trip against a real Postgres, exercising every adapter-consumed
/// Confiture subcommand: `current` → `up` → `verify` → `preflight` → `down-to`
/// (the surface in Confiture's `docs/reference/fraisier-adapter-contract.md`).
/// Opt-in via `FRAISIER_TEST_DATABASE_URL` (must be an *empty* database — the test
/// applies and rolls back migrations).
#[tokio::test]
async fn roundtrip_against_postgres_covers_full_surface() {
    let Ok(dsn) = std::env::var("FRAISIER_TEST_DATABASE_URL") else {
        eprintln!("skipping: set FRAISIER_TEST_DATABASE_URL to run the Postgres round-trip");
        return;
    };
    if !confiture_available().await {
        eprintln!("skipping: confiture not on PATH");
        return;
    }
    let _guard = ENV_LOCK.lock().await;

    let dir = std::env::temp_dir().join(format!("fraisier-rt-{}", std::process::id()));
    let migrations = dir.join("db/migrations");
    std::fs::create_dir_all(&migrations).expect("mkdir migrations");
    std::fs::write(
        migrations.join("001_init.up.sql"),
        "CREATE TABLE fraisier_rt_probe (id int primary key);\n",
    )
    .expect("write 001 up");
    std::fs::write(
        migrations.join("001_init.down.sql"),
        "DROP TABLE fraisier_rt_probe;\n",
    )
    .expect("write 001 down");
    std::fs::write(
        migrations.join("002_more.up.sql"),
        "ALTER TABLE fraisier_rt_probe ADD COLUMN note text;\n",
    )
    .expect("write 002 up");
    std::fs::write(
        migrations.join("002_more.down.sql"),
        "ALTER TABLE fraisier_rt_probe DROP COLUMN note;\n",
    )
    .expect("write 002 down");

    let source = "FRAISIER_CONF_IT_RT_DSN";
    std::env::set_var(source, &dsn);
    let mut ctx = AdapterCtx::new("checkout", "production");
    ctx.workdir = dir.clone();
    ctx.migrations_path = Some(migrations.clone());
    ctx.env_secrets
        .insert("DATABASE_URL".to_owned(), source.to_owned());

    let adapter = ConfitureMigration::new();

    // Fresh DB: no current revision (reachable but uninitialised → None).
    let before = adapter
        .current_revision(&ctx)
        .await
        .expect("current before");
    assert_eq!(before, None);

    // Apply all → current is 002.
    let up = adapter.up(&ctx, None).await.expect("up");
    assert_eq!(up.to, Some(Revision::new("002")));
    assert_eq!(
        adapter
            .current_revision(&ctx)
            .await
            .expect("current after up"),
        Some(Revision::new("002"))
    );

    // verify: every applied migration checks out (the contract's `failed_count == 0`).
    let verified = adapter.verify(&ctx).await.expect("verify");
    assert!(
        verified.ok,
        "verify should pass on cleanly-applied migrations; checks = {:?}",
        verified.checks
    );

    // preflight: the forward-compat lint must actually run against real Confiture.
    // Before Confiture 0.22, `migrate preflight` rejected the `--output` flag the
    // adapter passes to every subcommand, so this call could not produce a report at
    // all (it errored). 0.22 fixed that (CHANGELOG: "migrate preflight accepts
    // --output (fraisier adapter contract)"), so it now returns a report; our benign
    // DDL carries no Error-severity issue, so the report is clean.
    let preflight = adapter.preflight(&ctx).await.expect("preflight");
    assert!(
        preflight.ok,
        "preflight should be clean for benign DDL; issues = {:?}",
        preflight.issues
    );

    // Roll back to 001 → current is 001.
    let down = adapter
        .down_to(&ctx, Revision::new("001"))
        .await
        .expect("down_to");
    assert_eq!(down.to, Some(Revision::new("001")));
    assert_eq!(
        adapter
            .current_revision(&ctx)
            .await
            .expect("current after down"),
        Some(Revision::new("001"))
    );

    std::env::remove_var(source);
    let _ = std::fs::remove_dir_all(&dir);
}

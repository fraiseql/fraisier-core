//! Integration tests for the in-process Confiture adapter.
//!
//! These exercise the *real* `confiture` binary. They skip gracefully when it is
//! absent so the suite stays green on machines without Confiture installed. The
//! full Postgres round-trip runs only when `FRAISIER_TEST_DATABASE_URL` points at
//! a usable, empty database.

use fraisier_adapter_confiture::ConfitureMigration;
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterErrorKind, ChangeSetUnavailable, MigrationAdapter, Revision, RiskTier,
    RISK_CONTRACT_VERSION,
};
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

// ---------------------------------------------------------------------------
// Error-envelope handling, driven by a stand-in `confiture`
//
// Confiture's `fail()` boundary writes a structured *error envelope* to the
// `--output` file on **every** error path — the same file a successful run
// writes its report to. So "the adapter got JSON back" proves nothing about
// whether the command worked, and an envelope must never be read as a report.
//
// The payloads below were captured verbatim from confiture 0.37.0 (messages
// abridged); they are the three states a deploy actually hits.
// ---------------------------------------------------------------------------

/// Unreachable database — `migrate verify` exits 3.
#[cfg(unix)]
const ENVELOPE_UNREACHABLE: &str = r#"{
  "ok": false,
  "error": {
    "code": "CONFIG_006",
    "message": "Failed to connect to database: connection refused",
    "severity": "error",
    "details": {}, "migration": null, "file": null, "line": null,
    "actionable": "Check that the database server is running"
  }
}"#;

/// No migration ledger (a database built from schema files rather than
/// migrated) — `migrate verify` exits 2 under confiture 0.37.0.
#[cfg(unix)]
const ENVELOPE_NO_LEDGER: &str = r#"{
  "ok": false,
  "error": {
    "code": "PRECON_1001",
    "message": "No migration ledger found: `tb_confiture` is not present in this database",
    "severity": "error",
    "details": {}, "migration": null, "file": null, "line": null,
    "actionable": "Run `confiture migrate up`, or pass --allow-uninitialized"
  }
}"#;

/// The same ledger-less database under confiture 0.36.0, which exits 1. Kept so
/// the suite proves this was never a 0.37.0 regression.
#[cfg(unix)]
const ENVELOPE_INTERNAL: &str = r#"{
  "ok": false,
  "error": { "code": "INTERNAL_ERROR", "message": "relation \"tb_confiture\" does not exist" }
}"#;

/// A configuration problem that really is one — no usable DSN (`CONFIG_010`),
/// which confiture exits **5** for (the #146 renumbering moved config errors off
/// exit 2; exit 2 is now exclusively "no ledger").
#[cfg(unix)]
const ENVELOPE_BAD_CONFIG: &str = r#"{
  "ok": false,
  "error": { "code": "CONFIG_010", "message": "no usable database URL" }
}"#;

/// A *genuine* verify report in which checks failed. Not an envelope: it carries
/// the counts, so `ok ⇔ failed_count == 0` still applies.
#[cfg(unix)]
const REPORT_WITH_FAILURES: &str = r#"{
  "verified_count": 1, "failed_count": 1, "skipped_count": 0, "total_applied": 2,
  "results": [
    { "version": "001", "name": "init", "status": "verified", "error": null },
    { "version": "002", "name": "more", "status": "failed", "error": "checksum mismatch" }
  ]
}"#;

/// Linux's `ETXTBSY` — "text file busy", raised when exec'ing a file that some
/// process still holds open for writing.
#[cfg(unix)]
const ETXTBSY: i32 = 26;

/// The version the shared fake reports to `describe` unless a test asks for
/// another one. Old enough that it does not classify, so the capability-gated
/// tests have to opt in explicitly.
#[cfg(unix)]
const FAKE_DEFAULT_VERSION: &str = "0.39.0";

/// A fake `confiture` executable reporting `version`, written **once per
/// version** per test process.
///
/// Each script is deliberately written once and never rewritten. Writing an
/// executable and immediately exec'ing it from a multi-threaded process races
/// with any sibling test's `fork`: the child inherits the still-open write fd,
/// and the exec fails with `ETXTBSY` — which the adapter reports as a spawn
/// failure, silently turning an assertion about exit-code handling into an
/// assertion about nothing. Per-test data travels in files the script *reads*,
/// which removes that race rather than papering over it. The version is the one
/// input `describe` cannot pass that way (it sets no working directory), so it
/// keys the script instead of riding in it.
#[cfg(unix)]
fn fake_confiture_program_reporting(version: &str) -> std::path::PathBuf {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    static PROGRAMS: std::sync::Mutex<BTreeMap<String, std::path::PathBuf>> =
        std::sync::Mutex::new(BTreeMap::new());

    let mut programs = PROGRAMS.lock().expect("fake program registry");
    if let Some(script) = programs.get(version) {
        return script.clone();
    }

    let slug: String = version
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    let dir = std::env::temp_dir().join(format!("fraisier-fake-bin-{}-{slug}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir fake bin dir");
    std::fs::write(dir.join("version"), version).expect("write fake version");
    let script = dir.join("confiture");
    // Answers `--version` from the file beside it, and otherwise copies
    // ./payload.json to the path following `--output` and exits with
    // ./exit_code — both read from the per-call working directory the adapter
    // sets, which is how each test supplies its own scenario.
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         for arg in \"$@\"; do\n\
         \x20   if [ \"$arg\" = \"--version\" ]; then\n\
         \x20       echo \"confiture version $(cat \"$(dirname \"$0\")/version\")\"\n\
         \x20       exit 0\n\
         \x20   fi\n\
         done\n\
         out=\"\"\n\
         while [ $# -gt 0 ]; do\n\
         \x20   [ \"$1\" = \"--output\" ] && out=\"$2\"\n\
         \x20   shift\n\
         done\n\
         [ -n \"$out\" ] && cat payload.json > \"$out\"\n\
         exit \"$(cat exit_code)\"\n",
    )
    .expect("write fake confiture");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake confiture");

    // Burn the ETXTBSY window left by any sibling fork that inherited the write
    // fd. Those fds are O_CLOEXEC, so they clear as soon as the child execs;
    // this converges immediately in practice.
    for _ in 0..200 {
        match std::process::Command::new(&script)
            .arg("--version")
            .output()
        {
            Ok(_) => {
                programs.insert(version.to_owned(), script.clone());
                // The registry lock is held across the write *and* the burn-in
                // on purpose: it is what stops two threads racing to create the
                // same script, which is the race this whole helper exists to
                // avoid.
                drop(programs);
                return script;
            }
            Err(err) if err.raw_os_error() == Some(ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => panic!("fake confiture is not executable: {err}"),
        }
    }
    panic!("fake confiture stayed ETXTBSY");
}

/// A scenario for the shared fake: a working directory holding the payload it
/// should emit and the exit code it should return, reproducing Confiture's
/// failure boundary without needing a database (or Confiture itself).
#[cfg(unix)]
struct FakeConfiture {
    dir: std::path::PathBuf,
}

#[cfg(unix)]
impl FakeConfiture {
    fn new(tag: &str, payload: &str, code: i32) -> Self {
        let dir = std::env::temp_dir().join(format!("fraisier-fake-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir fake workdir");
        std::fs::write(dir.join("payload.json"), payload).expect("write payload");
        std::fs::write(dir.join("exit_code"), format!("{code}\n")).expect("write exit code");
        Self { dir }
    }

    /// The scenario with no report to write at all — a confiture that exits
    /// without producing its `--output` file.
    fn without_payload(self) -> Self {
        std::fs::remove_file(self.dir.join("payload.json")).expect("drop the payload");
        self
    }

    /// An adapter pointed at the shared fake. The scenario travels in the
    /// context's workdir, not in the program, which is why the program can be
    /// written once and shared.
    fn adapter() -> ConfitureMigration {
        Self::adapter_reporting(FAKE_DEFAULT_VERSION)
    }

    /// An adapter whose fake reports `version` to `describe`.
    fn adapter_reporting(version: &str) -> ConfitureMigration {
        ConfitureMigration::with_program(fake_confiture_program_reporting(version))
    }

    /// A context whose DSN is resolved in-process, so these tests mutate no
    /// environment and can run in parallel with the rest of the suite.
    fn ctx(&self) -> AdapterCtx {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.workdir.clone_from(&self.dir);
        ctx.migrations_path = Some(self.dir.clone());
        ctx.resolved_secrets.insert(
            "DATABASE_URL".to_owned(),
            "postgresql://localhost/unused".to_owned(),
        );
        ctx
    }
}

#[cfg(unix)]
impl Drop for FakeConfiture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The ship gate's load-bearing test: `verify` must never call an error a pass.
///
/// An envelope carries no `failed_count`, so reading it as a report yields zero
/// failures — i.e. `ok = true` — and the deploy gate goes green on a database
/// the adapter could not even reach.
#[cfg(unix)]
#[tokio::test]
async fn verify_never_reports_green_for_an_error_envelope() {
    for (tag, payload, code, state, detail) in [
        (
            "verify-unreachable",
            ENVELOPE_UNREACHABLE,
            3,
            "an unreachable database",
            "Failed to connect",
        ),
        (
            "verify-no-ledger",
            ENVELOPE_NO_LEDGER,
            2,
            "a ledger-less database on confiture 0.37.0",
            "No migration ledger",
        ),
        (
            "verify-internal",
            ENVELOPE_INTERNAL,
            1,
            "a ledger-less database on confiture 0.36.0",
            "does not exist",
        ),
    ] {
        let fake = FakeConfiture::new(tag, payload, code);
        match FakeConfiture::adapter().verify(&fake.ctx()).await {
            Ok(report) => panic!(
                "{state}: verify() read Confiture's error envelope as a report \
                 (ok = {}) — a silently-green ship gate",
                report.ok
            ),
            Err(err) => {
                assert_eq!(err.operation.as_deref(), Some("verify"));
                assert!(
                    err.message.contains(detail),
                    "{state}: the envelope's own message must reach the operator; got {}",
                    err.message
                );
            }
        }
    }
}

/// The contract the fix must not break: a verify report whose *checks* failed is
/// a valid result, not an adapter error — `ok ⇔ failed_count == 0`, whatever the
/// exit code. Only a non-report may become an error.
#[cfg(unix)]
#[tokio::test]
async fn verify_reports_genuine_check_failures_as_a_result() {
    let fake = FakeConfiture::new("verify-failed-checks", REPORT_WITH_FAILURES, 1);
    let report = FakeConfiture::adapter()
        .verify(&fake.ctx())
        .await
        .expect("a verify report with failed checks is a result, not an adapter error");

    assert!(!report.ok, "failed_count = 1 must mean ok = false");
    assert_eq!(report.checks.len(), 2);
    assert!(!report.checks[1].ok);
}

/// `preflight` shares `verify`'s return-JSON-before-checking-exit-code shape.
/// It failed *closed* rather than open — an envelope carries `"ok": false` — but
/// it reported a clean refusal: zero issues, and no trace of the unreachable
/// database that actually caused it.
#[cfg(unix)]
#[tokio::test]
async fn preflight_surfaces_the_envelope_instead_of_an_empty_refusal() {
    let fake = FakeConfiture::new("preflight-unreachable", ENVELOPE_UNREACHABLE, 3);
    let err = FakeConfiture::adapter()
        .preflight(&fake.ctx())
        .await
        .expect_err("an unreachable database is not a preflight verdict");

    assert!(
        err.message.contains("Failed to connect"),
        "the operator must be told why preflight could not run; got {}",
        err.message
    );
}

/// Exit 2 is not "your config is broken". Under confiture 0.37.0 a ledger-less
/// database exits 2 with `PRECON_1001`, which means "this database was never
/// migrated" — now its own `PreconditionFailed` kind. Reporting it as
/// `invalid_config` (JSON-RPC `-32602`) would send the operator to edit a config
/// file that is perfectly fine.
#[cfg(unix)]
#[tokio::test]
async fn uninitialised_database_is_a_precondition_not_an_invalid_config_error() {
    let fake = FakeConfiture::new("up-no-ledger", ENVELOPE_NO_LEDGER, 2);
    let err = FakeConfiture::adapter()
        .up(&fake.ctx(), None)
        .await
        .expect_err("up must fail against a ledger-less database");

    assert_eq!(
        err.kind,
        AdapterErrorKind::PreconditionFailed,
        "PRECON_1001 is an unmet precondition, its own kind — not Execution, not InvalidConfig"
    );
    assert_ne!(err.code, -32602);
}

// ---------------------------------------------------------------------------
// The migration risk contract, end to end
//
// The producer half lives in confiture (fraiseql/confiture#197); until it ships,
// the golden fixtures stand in for it — the same bytes both repositories test
// against. These drive the *whole* adapter path: spawn, `--output` file, report
// parse, and the typed change-set the policy gate will read.
// ---------------------------------------------------------------------------

/// The pact fixture with three tiers in one set.
#[cfg(unix)]
const FIXTURE_V1_MIXED: &str = include_str!("fixtures/preflight/v1-mixed.json");

/// The pre-contract payload: a confiture that emits no change-set at all.
#[cfg(unix)]
const FIXTURE_V0: &str = include_str!("fixtures/preflight/v0-no-change-set.json");

#[cfg(unix)]
#[tokio::test]
async fn preflight_surfaces_the_change_set_end_to_end() {
    let fake = FakeConfiture::new("preflight-change-set", FIXTURE_V1_MIXED, 0);
    let report = FakeConfiture::adapter()
        .preflight(&fake.ctx())
        .await
        .expect("a classified preflight report");

    // The fields that predate this contract are untouched.
    assert!(report.ok);
    assert_eq!(report.window_safe, Some(true));
    assert_eq!(report.issues.len(), 1);

    let set = report
        .usable_change_set()
        .expect("the change-set crosses the adapter seam");
    assert_eq!(set.contract_version, RISK_CONTRACT_VERSION);
    assert_eq!(set.changes.len(), 3);
    // Migration order, and every tier typed rather than inferred from a code.
    assert_eq!(set.changes[1].object, "public.tb_order.idx_placed_at");
    assert_eq!(set.changes[1].tier, Some(RiskTier::LockRisky));
    assert_eq!(set.changes[1].migration.as_deref(), Some("20260804120050"));
    assert_eq!(set.worst_tier(), Some(RiskTier::Irreversible));
    assert_eq!(set.unclassified().count(), 0);
}

/// The back-compat guarantee, driven through the real surface: a confiture that
/// predates the contract still produces a perfectly good report, and lands in
/// *did not classify* rather than *nothing to change*.
#[cfg(unix)]
#[tokio::test]
async fn a_pre_contract_confiture_still_preflights_but_never_classifies() {
    let fake = FakeConfiture::new("preflight-v0", FIXTURE_V0, 0);
    let report = FakeConfiture::adapter()
        .preflight(&fake.ctx())
        .await
        .expect("an old confiture still lints");

    assert!(report.ok);
    assert_eq!(report.window_safe, Some(true));
    assert_eq!(
        report.usable_change_set(),
        Err(ChangeSetUnavailable::NotEmitted),
        "no change-set is unclassified, which is never a clean bill of health"
    );
}

/// A confiture that exits cleanly but writes no report is an **error**, not an
/// empty green result — the same law as `verify`. Reading "no JSON" as "no
/// findings" would pass the gate on a preflight that never ran.
#[cfg(unix)]
#[tokio::test]
async fn a_confiture_that_writes_no_output_file_still_errors() {
    let fake = FakeConfiture::new("preflight-no-output", FIXTURE_V1_MIXED, 0).without_payload();
    let err = FakeConfiture::adapter()
        .preflight(&fake.ctx())
        .await
        .expect_err("a missing report is not a passing report");

    assert_eq!(err.operation.as_deref(), Some("preflight"));
}

/// The capability gate, through `describe` itself rather than the pure function
/// under it: what reaches the version comparison must be the *parsed* version,
/// not the raw `confiture version 0.40.0` line — which would fail to parse and
/// silently withhold the capability for ever. That is asserted directly on
/// `desc.version`, because while no released confiture classifies the capability
/// is withheld for *every* version and could no longer distinguish the two.
///
/// No confiture a user can install emits a change-set (fraiseql/confiture#197 is
/// open; 0.40.0–0.42.0 shipped without it), so `describe` must never advertise
/// `risk_tier` — the end-to-end statement of the `RISK_TIER_MIN_CONFITURE`
/// = `None` rule. When #197 releases and the floor is pinned, the `classifies`
/// column returns here with the real version in it.
#[cfg(unix)]
#[tokio::test]
async fn describe_advertises_risk_tier_only_when_the_binary_can_emit_it() {
    for (version, certifies_window) in [
        ("0.39.0", false),
        ("0.40.0", false),
        ("0.42.0", false),
        // The boundary of fraiseql/confiture#206, straddled: 0.43.0 emits a
        // `window_safe` that reads `true` for `DROP TABLE`, 0.44.0 does not.
        ("0.43.0", false),
        ("0.44.0", true),
        ("1.2.3", true),
    ] {
        let desc = FakeConfiture::adapter_reporting(version)
            .describe()
            .await
            .expect("the fake answers --version");

        assert_eq!(desc.version, version, "the reported version is parsed out");
        assert!(
            !desc.capabilities.iter().any(|cap| cap == "risk_tier"),
            "confiture {version} cannot emit a change-set: {:?}",
            desc.capabilities
        );
        // `preflight` never depends on the version — the lint has always run.
        assert!(desc.capabilities.iter().any(|cap| cap == "preflight"));
        // `window_safe` does, and this is the end-to-end statement of it: the
        // *parsed* version has to reach the floor, not the raw `--version` line.
        assert_eq!(
            desc.capabilities.iter().any(|cap| cap == "window_safe"),
            certifies_window,
            "confiture {version}: {:?}",
            desc.capabilities
        );
    }
}

/// ...but a *genuine* configuration problem — no usable DSN (`CONFIG_010`) —
/// exits 5 and stays `InvalidConfig`. A present, severe exit code is never
/// laundered into a benign precondition by a stray error code.
#[cfg(unix)]
#[tokio::test]
async fn a_genuine_config_error_stays_an_invalid_config_error() {
    let fake = FakeConfiture::new("up-bad-config", ENVELOPE_BAD_CONFIG, 5);
    let err = FakeConfiture::adapter()
        .up(&fake.ctx(), None)
        .await
        .expect_err("up must fail when Confiture has no usable DSN");

    assert_eq!(err.kind, AdapterErrorKind::InvalidConfig);
}

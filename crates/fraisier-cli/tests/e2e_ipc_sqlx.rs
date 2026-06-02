//! Capstone end-to-end: a `fraisier.toml` whose `[migration].adapter = "sqlx"`
//! deploys through the **real `fraisier` binary**, discovering the external
//! `fraisier-adapter-sqlx` reference adapter on `PATH` and driving it over the
//! IPC protocol — the first time the IPC migration adapter runs inside the full
//! saga deploy (Cycle 2.11).
//!
//! Unlike the in-workspace `src/e2e.rs` (which builds a `SingleHostDeploy`
//! directly from in-process adapters), this exercises the whole CLI path: argv →
//! config parse/validate → the [`factory`](../src/factory.rs) IPC branch (spawn
//! `fraisier-adapter-sqlx`, resolve the DSN env var and inject it as
//! `DATABASE_URL` on the child) → the saga. The artifact, health, and service
//! steps run for real against local fixtures; only `systemctl` is a fake script
//! (managing real units needs root).
//!
//! Two deploys tell the story:
//! 1. **v1, healthy** — applies migration `0001` over IPC and commits, recording
//!    the durable release ledger.
//! 2. **v2, unhealthy** — applies `0002`, then the health probe fails after
//!    activation, and the saga rolls back: it re-activates v1's artifact from the
//!    ledger and reverts `0002` by driving the sqlx adapter's `down_to` over IPC
//!    back to the live-captured pre-deploy revision.
//!
//! What this proves that the unit tests cannot:
//! - **`up(None)`**: a forward deploy sends no target, so the sqlx adapter (which
//!   declines a targeted `up`, having no `run_to`) applies all pending migrations.
//! - **Secret injection across two process boundaries**: the DSN reaches the
//!   adapter only as the `DATABASE_URL` env var resolved from a differently-named
//!   source var (`database_url_env`); a successful migration is the proof.
//! - **Rollback over the wire**: `down_to(previous_revision)` is the real IPC
//!   compensation, not an in-process fake.
//!
//! The sqlx adapter is a separate repository, so this test needs its built
//! binary. It is located via `FRAISIER_SQLX_ADAPTER_BIN`, else the conventional
//! sibling-repo build output (`../fraisier-adapter-sqlx/target/debug/…`). With
//! neither present the test skips with a diagnostic rather than failing — building
//! the sibling binary is a documented prerequisite (see `docs/DEMO.md`), not a
//! claim this suite can make on its own.

#![cfg(unix)]

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use fraisier_core::adapter_axes::{AdapterCtx, MigrationAdapter as _};
use fraisier_ipc::IpcMigrationAdapter;

/// The opaque bytes the fixture serves as the release artifact.
const ARTIFACT_BODY: &[u8] = b"fraiseql-v2-binary-payload";

/// The source env var the config reads the DSN from — deliberately *not*
/// `DATABASE_URL`, so the deploy proves the factory remaps it across the boundary.
const DSN_SOURCE_VAR: &str = "FRAISIER_E2E_SQLX_DSN";

// The migration corpus, written into the test's own directory so the test owns
// its fixtures (it borrows only the sibling repo's *binary*). Reversible, so the
// rollback's `down_to` has a real down step to run.
const M0001_UP: &str = "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);\n";
const M0001_DOWN: &str = "DROP TABLE widgets;\n";
const M0002_UP: &str = "ALTER TABLE widgets ADD COLUMN color TEXT;\n";
const M0002_DOWN: &str = "ALTER TABLE widgets DROP COLUMN color;\n";

/// Locate the built `fraisier-adapter-sqlx` binary (see the module docs).
fn sqlx_adapter_bin() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("FRAISIER_SQLX_ADAPTER_BIN") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Some(path);
        }
    }
    // crates/fraisier-cli → crates → <workspace root> → its parent holds the
    // sibling repo (both live side by side under the same code/ directory).
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2)?;
    let candidate = workspace
        .parent()?
        .join("fraisier-adapter-sqlx/target/debug/fraisier-adapter-sqlx");
    candidate.is_file().then_some(candidate)
}

/// Spawn a local HTTP fixture (mirrors `src/e2e.rs`): `*.sha256` → the artifact
/// digest, `/health` → the current `health` status (flippable to 500), anything
/// else → the artifact bytes.
fn spawn_fixture(sha: String, health: Arc<AtomicU16>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 2048];
            let read = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]);
            let path = request.split_whitespace().nth(1).unwrap_or("/");

            let (status, body): (u16, Vec<u8>) = if path.ends_with(".sha256") {
                (200, sha.clone().into_bytes())
            } else if path.contains("/health") {
                (health.load(Ordering::SeqCst), b"ok".to_vec())
            } else {
                (200, ARTIFACT_BODY.to_vec())
            };

            let header = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        }
    });
    addr
}

/// Write an executable fake `systemctl` answering `is-active` with `active`.
fn write_fake_systemctl(dir: &Path) -> PathBuf {
    let path = dir.join("systemctl");
    let script = "#!/bin/sh\ncase \"$1\" in is-active) echo active ;; esac\nexit 0\n";
    std::fs::write(&path, script).expect("write fake systemctl");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn sha256_hex(bytes: &[u8], dir: &Path) -> String {
    let input = dir.join("artifact-input");
    std::fs::write(&input, bytes).expect("write input");
    let output = Command::new("sha256sum")
        .arg(&input)
        .output()
        .expect("run sha256sum");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .expect("digest")
        .to_owned()
}

fn write_migration(dir: &Path, stem: &str, up: &str, down: &str) {
    std::fs::write(dir.join(format!("{stem}.up.sql")), up).expect("write up");
    std::fs::write(dir.join(format!("{stem}.down.sql")), down).expect("write down");
}

/// The file name the `active_path` symlink currently points at (the live release).
fn live_release(active: &Path) -> String {
    std::fs::read_link(active)
        .expect("active symlink exists")
        .file_name()
        .and_then(|n| n.to_str())
        .expect("link name")
        .to_owned()
}

/// The DSN for a SQLite database file (rwc: read-write, create if absent).
fn dsn_for(db: &Path) -> String {
    format!("sqlite://{}?mode=rwc", db.display())
}

/// Ask the sqlx adapter, over the real IPC wire, for the highest applied revision
/// — the DB-level check that a deploy (or its rollback) left the schema where we
/// expect.
async fn current_revision(bin: &Path, dsn: &str, migrations: &Path) -> Option<String> {
    let mut ctx = AdapterCtx::new("fraiseql", "production");
    ctx.migrations_path = Some(migrations.to_path_buf());
    IpcMigrationAdapter::new(bin, "sqlx")
        .with_env("DATABASE_URL", dsn)
        .current_revision(&ctx)
        .await
        .expect("current_revision over IPC")
        .map(|rev| rev.0)
}

/// One `fraisier deploy --json` invocation against the real binary, with the sqlx
/// adapter on `PATH`, the DSN in the source env var, and the fake `systemctl`
/// wired in. Returns `(exit_code, parsed_json)`.
fn run_deploy(
    env: &DeployEnv,
    config: &Path,
    state: &Path,
    version: &str,
) -> (i32, serde_json::Value) {
    let sqlx_dir = env
        .sqlx_bin
        .parent()
        .expect("adapter binary has a parent directory");
    let path_var = std::env::var("PATH").unwrap_or_default();

    let output = Command::new(&env.fraisier_bin)
        .arg("--json")
        .arg("deploy")
        .arg("--config")
        .arg(config)
        .arg("--state-dir")
        .arg(state)
        .arg("--app-version")
        .arg(version)
        .env("PATH", format!("{}:{path_var}", sqlx_dir.display()))
        .env(DSN_SOURCE_VAR, &env.dsn)
        .env("FRAISIER_SYSTEMCTL_BIN", &env.fake_systemctl)
        .output()
        .expect("spawn fraisier");

    let code = output.status.code().expect("fraisier exited normally");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "deploy stdout was not JSON ({e}):\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (code, json)
}

/// The fixed parts of the deploy environment, threaded through both invocations.
struct DeployEnv {
    fraisier_bin: PathBuf,
    sqlx_bin: PathBuf,
    fake_systemctl: PathBuf,
    dsn: String,
}

#[tokio::test]
async fn sqlx_ipc_adapter_deploys_then_rolls_back_through_the_cli() {
    let Some(sqlx_bin) = sqlx_adapter_bin() else {
        eprintln!(
            "SKIP sqlx_ipc_adapter_deploys_then_rolls_back_through_the_cli: \
             fraisier-adapter-sqlx binary not found. Build it \
             (`cargo build` in the fraisier-adapter-sqlx repo) or set \
             FRAISIER_SQLX_ADAPTER_BIN. See docs/DEMO.md."
        );
        return;
    };
    let fraisier_bin = PathBuf::from(env!("CARGO_BIN_EXE_fraisier"));

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let bin_dir = root.join("bin");
    let migrations = root.join("migrations");
    let staging = root.join("staging");
    let active = root.join("current");
    let state = root.join("state");
    let db = root.join("app.db");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    std::fs::create_dir_all(&migrations).expect("migrations dir");

    // v1 ships migration 0001 only.
    write_migration(&migrations, "0001_create_widgets", M0001_UP, M0001_DOWN);

    let sha = sha256_hex(ARTIFACT_BODY, root);
    let health = Arc::new(AtomicU16::new(200));
    let addr = spawn_fixture(sha, Arc::clone(&health));
    let fake_systemctl = write_fake_systemctl(&bin_dir);
    let dsn = dsn_for(&db);

    let config = root.join("fraisier.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[deploy]
name = "fraiseql"
environment = "production"

[artifact]
source = "release"
release_url = "http://{addr}/app-{{version}}.tar.gz"
checksum_url = "http://{addr}/app-{{version}}.tar.gz.sha256"
staging_dir = "{staging}"
active_path = "{active}"

[migration]
adapter = "sqlx"
database_url_env = "{DSN_SOURCE_VAR}"
migrations_path = "{migrations}"

[service]
adapter = "systemd"
unit = "fraiseql.service"

[health]
adapter = "http"
url = "http://{addr}/health"
expected_status = 200
"#,
            staging = staging.display(),
            active = active.display(),
            migrations = migrations.display(),
        ),
    )
    .expect("write config");

    let env = DeployEnv {
        fraisier_bin,
        sqlx_bin,
        fake_systemctl,
        dsn,
    };

    // --- Deploy #1: v1, healthy → applies 0001 over IPC and commits. ---
    let (code, json) = run_deploy(&env, &config, &state, "v1");
    assert_eq!(code, 0, "deploy #1 should commit; json: {json}");
    assert_eq!(json["outcome"], serde_json::json!("committed"));
    assert_eq!(
        current_revision(&env.sqlx_bin, &env.dsn, &migrations).await,
        Some("1".to_owned()),
        "0001 applied through the IPC adapter",
    );
    assert_eq!(live_release(&active), "v1", "v1 is the live release");

    // --- v2 adds migration 0002; the next deploy's health will fail. ---
    write_migration(&migrations, "0002_add_color", M0002_UP, M0002_DOWN);
    health.store(500, Ordering::SeqCst);

    // --- Deploy #2: v2, unhealthy → applies 0002, then rolls back. ---
    let (code, json) = run_deploy(&env, &config, &state, "v2");
    assert_eq!(code, 1, "deploy #2 should roll back; json: {json}");
    assert_eq!(json["outcome"], serde_json::json!("rolled_back"));
    assert!(
        json["detail"]
            .as_str()
            .is_some_and(|d| d.contains("health")),
        "rollback was triggered by the health step; json: {json}",
    );

    // The rollback drove the sqlx adapter's `down_to` over IPC back to revision 1...
    assert_eq!(
        current_revision(&env.sqlx_bin, &env.dsn, &migrations).await,
        Some("1".to_owned()),
        "0002 was reverted through the IPC adapter's down_to",
    );
    // ...and re-activated v1's artifact from the durable ledger.
    assert_eq!(
        live_release(&active),
        "v1",
        "rollback re-activated the prior release",
    );
}

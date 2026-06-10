//! End-to-end single-host deploy against the **real** adapters and local
//! fixtures — a reproducible, no-infrastructure demo of the deploy saga.
//!
//! It exercises the whole stack with no external services and no root:
//! `ReleaseArtifact` really downloads + sha256-verifies + symlink-activates from a
//! local HTTP server; `CommandMigration` really runs `sh -c` migration commands;
//! `HttpHealth` really probes; only `systemctl` is a fake script on disk (managing
//! real units needs root — the honest stand-in). The narrative is two deploys:
//! the first commits and records the durable release ledger; the second is forced
//! to fail its health check after activation, and the saga rolls back by
//! re-activating the **previously-active artifact** from the ledger.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use fraisier_adapter_command::{CommandHealth, CommandMigration};
use fraisier_adapter_http::HttpHealth;
use fraisier_adapter_systemd::SystemdService;
use fraisier_artifact_release::ReleaseArtifact;
use fraisier_core::adapter_axes::{AdapterCtx, HealthAdapter, HostId};
use fraisier_core::single_host::SingleHostDeploy;
use fraisier_saga::saga::SagaOutcome;
use fraisier_saga::state_store::FilesystemStateStore;
use serde_json::json;

/// The bytes the fixture serves as the release artifact (opaque to the adapter).
const ARTIFACT_BODY: &[u8] = b"fraiseql-v2-binary-payload";

/// Spawn a local HTTP server that answers, on an ephemeral port:
/// - `*.sha256` → the artifact's hex digest,
/// - `/health`  → the current value of `health` (so the test can flip it to 500),
/// - anything else → the artifact bytes.
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

/// Write an executable fake `systemctl` that logs its argv and answers
/// `is-active` with `active`.
fn write_fake_systemctl(dir: &Path, log: &Path) -> PathBuf {
    let path = dir.join("systemctl");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> \"{}\"\ncase \"$1\" in is-active) echo active ;; esac\nexit 0\n",
        log.display()
    );
    std::fs::write(&path, script).expect("write fake systemctl");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// A `CommandMigration` whose `up` records `up:<version>` and stores `<version>`
/// as the current revision; `down_to` records `down:<target>` and rolls the
/// stored revision back to the target. This is a real `sh` migration with no DB.
fn migration(version: &str, log: &Path, revfile: &Path) -> CommandMigration {
    let log = log.display();
    let rev = revfile.display();
    let mut settings = BTreeMap::new();
    settings.insert(
        "commands".to_owned(),
        json!({
            "current_revision": format!("cat \"{rev}\" 2>/dev/null || true"),
            "up": format!("printf 'up:%s\\n' \"{version}\" >> \"{log}\"; printf '%s\\n' \"{version}\" > \"{rev}\""),
            "down_to": format!("printf 'down:%s\\n' \"$FRAISIER_TARGET\" >> \"{log}\"; printf '%s\\n' \"$FRAISIER_TARGET\" > \"{rev}\""),
            "verify": "true",
        }),
    );
    CommandMigration::from_settings("command", &settings)
}

/// The shared adapter context for one deploy at `version`.
fn deploy_ctx(version: &str, addr: SocketAddr, staging: &Path, active: &Path) -> AdapterCtx {
    let mut ctx = AdapterCtx::new("fraiseql", "production");
    ctx.host = Some(HostId::new("localhost"));
    let settings = &mut ctx.settings;
    settings.insert(
        "release_url".to_owned(),
        json!(format!("http://{addr}/app-{{version}}.tar.gz")),
    );
    settings.insert(
        "checksum_url".to_owned(),
        json!(format!("http://{addr}/app-{{version}}.tar.gz.sha256")),
    );
    settings.insert("version".to_owned(), json!(version));
    settings.insert(
        "staging_dir".to_owned(),
        json!(staging.display().to_string()),
    );
    settings.insert(
        "active_path".to_owned(),
        json!(active.display().to_string()),
    );
    settings.insert("unit".to_owned(), json!("demo.service"));
    settings.insert("url".to_owned(), json!(format!("http://{addr}/health")));
    settings.insert("expected_status".to_owned(), json!(200));
    settings.insert("attempts".to_owned(), json!(1));
    settings.insert("retry_delay_ms".to_owned(), json!(0));
    settings.insert("timeout_ms".to_owned(), json!(2000));
    ctx
}

fn sha256_hex(bytes: &[u8], dir: &Path) -> String {
    let input = dir.join("artifact-input");
    std::fs::write(&input, bytes).expect("write input");
    let output = std::process::Command::new("sha256sum")
        .arg(&input)
        .output()
        .expect("run sha256sum");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .expect("digest")
        .to_owned()
}

fn link_target_name(active: &Path) -> String {
    std::fs::read_link(active)
        .expect("active symlink exists")
        .file_name()
        .and_then(|n| n.to_str())
        .expect("link name")
        .to_owned()
}

/// The absolute path to the committed perf-scan stub (`scripts/perf-scan-stub.sh`),
/// resolved from this crate's manifest dir so the test is CWD-independent.
fn perf_scan_stub() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/perf-scan-stub.sh")
        .canonicalize()
        .expect("perf-scan stub exists")
}

/// Build and run one single-host deploy at `version` with the given context and
/// health adapter, against the shared local fixtures (real release artifact + `sh`
/// migration + fake `systemctl`). Returns the saga outcome.
async fn run_deploy(
    version: &str,
    ctx: AdapterCtx,
    health: Arc<dyn HealthAdapter>,
    log: &Path,
    revfile: &Path,
    systemctl: &Path,
    store: FilesystemStateStore,
) -> SagaOutcome {
    SingleHostDeploy::builder("fraiseql", "production", HostId::new("localhost"))
        .context(ctx)
        .artifact(Arc::new(ReleaseArtifact::new()))
        .migration(Arc::new(migration(version, log, revfile)))
        .service(Arc::new(SystemdService::with_program(systemctl)))
        .health(health)
        .build()
        .expect("build deploy")
        .run(store)
        .await
        .expect("run deploy")
}

#[tokio::test]
async fn deploy_commits_then_rolls_back_to_the_prior_release() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let staging = root.join("staging");
    let active = root.join("current");
    let log = root.join("calls.log");
    let revfile = root.join("revision");
    let state_dir = root.join("state");

    let sha = sha256_hex(ARTIFACT_BODY, root);
    let health = Arc::new(AtomicU16::new(200));
    let addr = spawn_fixture(sha, Arc::clone(&health));
    let systemctl = write_fake_systemctl(root, &log);

    let store = FilesystemStateStore::new(&state_dir).expect("store");

    // --- Deploy #1: healthy → commits and records the release ledger. ---
    let outcome = run_deploy(
        "v1",
        deploy_ctx("v1", addr, &staging, &active),
        Arc::new(HttpHealth::new()),
        &log,
        &revfile,
        &systemctl,
        store.clone(),
    )
    .await;
    assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");
    assert_eq!(link_target_name(&active), "v1", "v1 is live");
    assert_eq!(
        std::fs::read_to_string(&revfile).expect("revfile").trim(),
        "v1",
        "the migration advanced to v1",
    );

    // --- Deploy #2: health now fails after activation → rollback to v1. ---
    health.store(500, Ordering::SeqCst);
    let outcome = run_deploy(
        "v2",
        deploy_ctx("v2", addr, &staging, &active),
        Arc::new(HttpHealth::new()),
        &log,
        &revfile,
        &systemctl,
        store,
    )
    .await;
    assert!(
        matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "health"),
        "got {outcome:?}",
    );

    // The durable ledger restored the previously-active artifact...
    assert_eq!(
        link_target_name(&active),
        "v1",
        "rollback re-activated the prior release through the ledger",
    );
    // ...and the migration rolled the database back to the pre-deploy revision.
    assert_eq!(
        std::fs::read_to_string(&revfile).expect("revfile").trim(),
        "v1",
        "down_to returned the migration to v1",
    );

    let calls = std::fs::read_to_string(&log).expect("log");
    assert!(calls.contains("up:v1"), "v1 applied: {calls}");
    assert!(
        calls.contains("up:v2"),
        "v2 applied before the failure: {calls}"
    );
    assert!(calls.contains("down:v1"), "rolled back to v1: {calls}");
    assert!(
        calls.contains("restart demo.service"),
        "service restarted: {calls}"
    );
}

/// The command health gate is the honest post-deploy perf-regression rollback:
/// the saga `Health` step runs the perf-scan stub, and when it exits non-zero
/// (a regression under `--fail-on-regression`) the deploy rolls back with the
/// scan's excerpt carried into `RolledBack.reason`. Mirrors the HTTP narrative —
/// a healthy first deploy then a regressed second — because a *first*-ever deploy
/// has no prior release to compensate the activation against.
#[tokio::test]
async fn command_health_regression_rolls_back_naming_the_detail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let staging = root.join("staging");
    let active = root.join("current");
    let log = root.join("calls.log");
    let revfile = root.join("revision");
    let state_dir = root.join("state");

    let sha = sha256_hex(ARTIFACT_BODY, root);
    // The gate is the command, not the fixture; the fixture still serves the
    // artifact + checksum, so its health value is unused by CommandHealth.
    let addr = spawn_fixture(sha, Arc::new(AtomicU16::new(200)));
    let systemctl = write_fake_systemctl(root, &log);
    let store = FilesystemStateStore::new(&state_dir).expect("store");
    let stub = perf_scan_stub();

    // A `[health].command` string that runs the stub; REGRESS toggles the gate.
    // The DSN would travel by env (none needed: the stub has no DB).
    let scan = |regress: bool| {
        let prefix = if regress { "REGRESS=1 " } else { "" };
        json!(format!(
            "{prefix}bash {} regression-scan --fail-on-regression",
            stub.display(),
        ))
    };

    // --- Deploy #1: the scan finds no regression (exit 0) → commits. ---
    let mut ctx = deploy_ctx("v1", addr, &staging, &active);
    ctx.settings.insert("command".to_owned(), scan(false));
    let outcome = run_deploy(
        "v1",
        ctx,
        Arc::new(CommandHealth::new()),
        &log,
        &revfile,
        &systemctl,
        store.clone(),
    )
    .await;
    assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");

    // --- Deploy #2: the scan finds a regression (exit 1) → rollback to v1. ---
    let mut ctx = deploy_ctx("v2", addr, &staging, &active);
    ctx.settings.insert("command".to_owned(), scan(true));
    let outcome = run_deploy(
        "v2",
        ctx,
        Arc::new(CommandHealth::new()),
        &log,
        &revfile,
        &systemctl,
        store,
    )
    .await;

    let SagaOutcome::RolledBack {
        failed_step,
        reason,
    } = &outcome
    else {
        panic!("expected a rollback, got {outcome:?}");
    };
    assert_eq!(failed_step, "health", "the health gate failed the deploy");
    assert!(
        reason.contains("order/UPDATE"),
        "the rollback reason carries the perf-scan detail: {reason}",
    );
    assert_eq!(
        link_target_name(&active),
        "v1",
        "rollback re-activated the prior release",
    );
}

//! Integration tests for the systemd service adapter.
//!
//! They drive a *fake* `systemctl` script (written to a temp file) so no real
//! systemd or privilege is needed — the script emulates `is-active`/`restart`
//! exit codes and output.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use fraisier_adapter_systemd::SystemdService;
use fraisier_core::adapter_axes::{AdapterCtx, AdapterErrorKind, HostId, ServiceAdapter};
use serde_json::json;

/// Per-call discriminator so concurrent tests in this binary never share a path
/// (each test removes its own file at the end; a shared path would let one test
/// delete the binary another is still spawning).
static FAKE_SEQ: AtomicU32 = AtomicU32::new(0);

/// Write a fake `systemctl` to a unique temp path and return it.
fn fake_systemctl() -> PathBuf {
    let unique = FAKE_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "fraisier-fake-systemctl-{}-{unique}",
        std::process::id(),
    ));
    let script = "#!/bin/sh\n\
        case \"$1\" in --user) shift;; esac\n\
        verb=\"$1\"; unit=\"$2\"\n\
        case \"$verb\" in\n\
        is-active) case \"$unit\" in *active*) printf 'active\\n'; exit 0;; \
                   *failed*) printf 'failed\\n'; exit 3;; \
                   *) printf 'inactive\\n'; exit 3;; esac;;\n\
        restart) case \"$unit\" in *fail*) printf 'Job failed\\n' 1>&2; exit 1;; *) exit 0;; esac;;\n\
        *) exit 2;; esac\n";
    let mut file = std::fs::File::create(&path).expect("create fake systemctl");
    file.write_all(script.as_bytes()).expect("write script");
    let mut perms = file.metadata().expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

/// A fake `systemctl` that appends each `"<verb> <unit>"` it sees to `log` so a
/// test can assert the call order. `reset-failed` exits non-zero here to prove
/// it is best-effort (a benign failure must not block the restart); `restart`
/// succeeds.
fn logging_fake_systemctl(log: &std::path::Path) -> PathBuf {
    let unique = FAKE_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "fraisier-log-systemctl-{}-{unique}",
        std::process::id(),
    ));
    let script = format!(
        "#!/bin/sh\n\
        case \"$1\" in --user) shift;; esac\n\
        printf '%s %s\\n' \"$1\" \"$2\" >> '{}'\n\
        case \"$1\" in\n\
        reset-failed) exit 1;;\n\
        restart) exit 0;;\n\
        *) exit 2;; esac\n",
        log.display(),
    );
    let mut file = std::fs::File::create(&path).expect("create logging systemctl");
    file.write_all(script.as_bytes()).expect("write script");
    let mut perms = file.metadata().expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

fn ctx(unit: &str) -> AdapterCtx {
    let mut ctx = AdapterCtx::new("checkout", "production");
    ctx.settings.insert("unit".to_owned(), json!(unit));
    ctx
}

fn host() -> HostId {
    HostId::new("web-1")
}

#[tokio::test]
async fn restart_succeeds_and_failure_is_error() {
    let bin = fake_systemctl();
    let adapter = SystemdService::with_program(&bin);

    adapter
        .restart(&ctx("fraiseql-active.service"), &host())
        .await
        .expect("restart of a healthy unit succeeds");

    let err = adapter
        .restart(&ctx("fail.service"), &host())
        .await
        .expect_err("a failing restart is an error");
    assert_eq!(err.kind, AdapterErrorKind::Execution);
    assert_eq!(err.adapter.as_deref(), Some("systemd"));

    let _ = std::fs::remove_file(&bin);
}

#[tokio::test]
async fn restart_resets_failed_state_before_restarting() {
    // A unit that hit systemd's start rate limit (e.g. a rollback restart right
    // after a failed deploy restart) is refused as "start request repeated too
    // quickly" until `reset-failed` clears the counter. The adapter must clear
    // that state first — and tolerate `reset-failed` itself failing.
    let unique = FAKE_SEQ.fetch_add(1, Ordering::Relaxed);
    let log = std::env::temp_dir().join(format!(
        "fraisier-systemctl-log-{}-{unique}",
        std::process::id(),
    ));
    let _ = std::fs::remove_file(&log);
    let bin = logging_fake_systemctl(&log);
    let adapter = SystemdService::with_program(&bin);

    adapter
        .restart(&ctx("fraiseql.service"), &host())
        .await
        .expect("restart succeeds even though reset-failed returned non-zero");

    let calls = std::fs::read_to_string(&log).expect("read call log");
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        vec!["reset-failed fraiseql.service", "restart fraiseql.service"],
        "restart must reset-failed first, then restart",
    );

    let _ = std::fs::remove_file(&bin);
    let _ = std::fs::remove_file(&log);
}

#[tokio::test]
async fn status_reflects_is_active() {
    let bin = fake_systemctl();
    let adapter = SystemdService::with_program(&bin);

    let up = adapter
        .status(&ctx("fraiseql-active.service"), &host())
        .await
        .expect("status");
    assert!(up.running);
    assert_eq!(up.detail.as_deref(), Some("active"));

    let down = adapter
        .status(&ctx("fraiseql-down.service"), &host())
        .await
        .expect("status");
    assert!(!down.running);
    assert_eq!(down.detail.as_deref(), Some("inactive"));

    let _ = std::fs::remove_file(&bin);
}

#[tokio::test]
async fn missing_unit_is_invalid_config() {
    let adapter = SystemdService::with_program("/bin/true");
    let err = adapter
        .restart(&AdapterCtx::new("checkout", "production"), &host())
        .await
        .expect_err("no unit configured");
    assert_eq!(err.kind, AdapterErrorKind::InvalidConfig);
}

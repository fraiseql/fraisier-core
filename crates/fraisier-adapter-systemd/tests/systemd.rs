//! Integration tests for the systemd service adapter.
//!
//! They drive a *fake* `systemctl` script (written to a temp file) so no real
//! systemd or privilege is needed — the script emulates `is-active`/`restart`
//! exit codes and output.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use fraisier_adapter_systemd::SystemdService;
use fraisier_core::adapter_axes::{AdapterCtx, AdapterErrorKind, HostId, ServiceAdapter};
use serde_json::json;

/// Write a fake `systemctl` to a unique temp path and return it.
fn fake_systemctl() -> PathBuf {
    let path = std::env::temp_dir().join(format!("fraisier-fake-systemctl-{}", std::process::id()));
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

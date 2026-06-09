//! Integration tests for `NginxLb`: real config-file edits + a fake `nginx`
//! binary, so the drain/reattach round-trip needs no real nginx.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use fraisier_adapter_nginx::NginxLb;
use fraisier_core::adapter_axes::{AdapterCtx, HostId, LbAdapter, LbState};
use serde_json::json;

const CONFIG: &str = "\
upstream fraiseql_upstream {
    server web1.internal:8080 weight=5;
    server web2.internal:8080;
}
";

/// Write a fake `nginx` that records its argv and exits 0.
fn fake_nginx(dir: &Path, log: &Path) -> PathBuf {
    let path = dir.join("nginx");
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho \"$@\" >> \"{}\"\nexit 0\n", log.display()),
    )
    .expect("write fake nginx");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn ctx(config_path: &Path, address: &str) -> AdapterCtx {
    let mut ctx = AdapterCtx::new("fraiseql", "production");
    ctx.settings.insert(
        "config_path".to_owned(),
        json!(config_path.display().to_string()),
    );
    ctx.settings
        .insert("upstream".to_owned(), json!("fraiseql_upstream"));
    ctx.settings.insert("address".to_owned(), json!(address));
    ctx
}

#[tokio::test]
async fn drain_then_reattach_round_trips_the_host() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("fraiseql.conf");
    std::fs::write(&config, CONFIG).expect("write config");
    let log = dir.path().join("nginx.log");
    let lb = NginxLb::with_program(fake_nginx(dir.path(), &log));
    let ctx = ctx(&config, "web1.internal");
    let host = HostId::new("web-1");

    // Drain: the host is marked down, its prior in-pool membership is returned.
    let prior = lb.drain(&ctx, &host).await.expect("drain");
    assert_eq!(prior.state, LbState::InPool);
    assert_eq!(prior.weight, Some(5));
    let after_drain = std::fs::read_to_string(&config).expect("read");
    assert!(
        after_drain.contains("server web1.internal:8080 weight=5 down;"),
        "got:\n{after_drain}"
    );
    // The other host is untouched.
    assert!(after_drain.contains("server web2.internal:8080;"));

    // Reattach: the down flag is cleared, restoring the prior membership.
    lb.reattach(&ctx, &host, &prior).await.expect("reattach");
    let after_reattach = std::fs::read_to_string(&config).expect("read");
    assert!(
        after_reattach.contains("server web1.internal:8080 weight=5;")
            && !after_reattach.contains("down"),
        "got:\n{after_reattach}"
    );

    // A backup of the prior config was kept, and nginx was reloaded twice.
    assert!(
        config.with_extension("conf.bak").exists() || dir.path().join("fraiseql.conf.bak").exists()
    );
    let reloads = std::fs::read_to_string(&log).expect("log");
    assert_eq!(reloads.matches("-s reload").count(), 2, "log:\n{reloads}");
}

#[tokio::test]
async fn draining_an_unknown_host_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("fraiseql.conf");
    std::fs::write(&config, CONFIG).expect("write config");
    let log = dir.path().join("nginx.log");
    let lb = NginxLb::with_program(fake_nginx(dir.path(), &log));

    let err = lb
        .drain(&ctx(&config, "nope.internal"), &HostId::new("nope"))
        .await
        .expect_err("unknown host");
    assert!(
        err.message.contains("not found in upstream"),
        "{}",
        err.message
    );
    // Nothing was reloaded.
    assert!(!log.exists() || std::fs::read_to_string(&log).unwrap_or_default().is_empty());
}

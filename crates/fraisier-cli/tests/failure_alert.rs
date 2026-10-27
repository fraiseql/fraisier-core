//! The failure-record handoff through the real binary, as systemd drives it: a
//! scheduled backup fails with `--failure-record` under an `$INVOCATION_ID`,
//! and the alert unit's `fraisier notify` finds the record by
//! `$MONITOR_INVOCATION_ID` and sends it once.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

const ID: &str = "4f2c0a8e9b1d4e7f8a6c5b3d2e1f0a9b";
const UNIT: &str = "fraisier-checkout-production-scheduled.service";

fn fraisier(runtime: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fraisier"));
    command
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .env_remove("INVOCATION_ID")
        .env_remove("MONITOR_INVOCATION_ID");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("spawn fraisier")
}

#[test]
fn a_failed_backup_is_sent_once_by_its_alert() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runtime = tmp.path().join("runtime");
    let hook_log = tmp.path().join("hook.log");
    let config = tmp.path().join("fraisier.toml");
    // No [migration] database: the backup fails before reaching pg_dump.
    std::fs::write(
        &config,
        format!(
            "[deploy]\nname = \"checkout\"\nenvironment = \"production\"\n\n\
             [notify]\ncommand = 'printf \"%s|%s\\n\" \"$FRAISIER_NOTIFY_EVENT\" \
             \"$FRAISIER_NOTIFY_REASON\" >> {}'\n",
            hook_log.display()
        ),
    )
    .expect("config");
    let config = config.to_str().expect("utf-8 path");

    let backup = fraisier(
        &runtime,
        &["backup", "--config", config, "--failure-record", UNIT],
        &[("INVOCATION_ID", ID)],
    );
    assert!(!backup.status.success(), "the backup must fail");
    let record = runtime.join(format!("fraisier/failures/{ID}.json"));
    assert!(
        record.exists(),
        "record written; stderr: {}",
        String::from_utf8_lossy(&backup.stderr)
    );
    assert!(!hook_log.exists(), "the failing run itself sends nothing");

    let alert = fraisier(
        &runtime,
        &["notify", "--config", config, "--unit", UNIT],
        &[("MONITOR_INVOCATION_ID", ID)],
    );
    assert!(
        alert.status.success(),
        "{}",
        String::from_utf8_lossy(&alert.stderr)
    );
    let sent = std::fs::read_to_string(&hook_log).expect("hook ran");
    assert_eq!(
        sent.matches("scheduled-backup-failed|").count(),
        1,
        "{sent}"
    );
    assert!(sent.starts_with("scheduled-backup-failed|"), "{sent}");
    assert!(!sent.contains("no failure record"), "{sent}");
    assert!(!record.exists(), "the record is consumed");

    // A second firing for the same activation finds no record and says so; it
    // never resends the reason.
    let again = fraisier(
        &runtime,
        &["notify", "--config", config, "--unit", UNIT],
        &[("MONITOR_INVOCATION_ID", ID)],
    );
    assert!(again.status.success());
    let sent = std::fs::read_to_string(&hook_log).expect("hook ran");
    assert_eq!(
        sent.matches("scheduled-backup-failed|").count(),
        2,
        "{sent}"
    );
    assert!(
        sent.contains("|no failure record; see journalctl"),
        "{sent}"
    );
}

#[test]
fn without_failure_record_a_failed_backup_leaves_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runtime = tmp.path().join("runtime");
    let config = tmp.path().join("fraisier.toml");
    std::fs::write(
        &config,
        "[deploy]\nname = \"checkout\"\nenvironment = \"production\"\n",
    )
    .expect("config");
    let backup = fraisier(
        &runtime,
        &["backup", "--config", config.to_str().expect("utf-8")],
        &[("INVOCATION_ID", ID)],
    );
    assert!(!backup.status.success());
    assert!(!runtime.exists(), "nothing is sniffed from the environment");
}

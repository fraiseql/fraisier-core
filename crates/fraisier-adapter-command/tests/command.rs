//! Integration tests for the command (escape-hatch) migration adapter.
//!
//! These run real `sh`/`printf`/`true`/`false` subprocesses — always available
//! on the target platform, so the suite needs no external service.

use std::collections::BTreeMap;

use fraisier_adapter_command::CommandMigration;
use fraisier_core::adapter_axes::{AdapterCtx, AdapterErrorKind, MigrationAdapter, Revision};
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// `set_var`/`var` are process-global; serialise the secret-injection test.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

fn adapter(commands: Value) -> CommandMigration {
    let mut settings = BTreeMap::new();
    settings.insert("commands".to_owned(), commands);
    CommandMigration::from_settings("command", &settings)
}

fn ctx() -> AdapterCtx {
    AdapterCtx::new("checkout", "production")
}

#[tokio::test]
async fn current_revision_trims_stdout() {
    let a = adapter(json!({ "current_revision": "printf '  20260601_abc \n'" }));
    let rev = a.current_revision(&ctx()).await.expect("current");
    assert_eq!(rev, Some(Revision::new("20260601_abc")));
}

#[tokio::test]
async fn current_revision_empty_is_none() {
    let a = adapter(json!({ "current_revision": "true" }));
    assert_eq!(a.current_revision(&ctx()).await.expect("current"), None);
}

#[tokio::test]
async fn unconfigured_method_is_invalid_config() {
    let a = adapter(json!({ "up": "true" }));
    let err = a
        .current_revision(&ctx())
        .await
        .expect_err("no current_revision command configured");
    assert_eq!(err.kind, AdapterErrorKind::InvalidConfig);
}

#[tokio::test]
async fn up_succeeds_and_failure_is_error() {
    let ok = adapter(json!({ "up": "true" }));
    let outcome = ok
        .up(&ctx(), Some(Revision::new("003")))
        .await
        .expect("up ok");
    assert_eq!(outcome.to, Some(Revision::new("003")));

    let bad = adapter(json!({ "up": "false" }));
    let err = bad
        .up(&ctx(), None)
        .await
        .expect_err("non-zero up is an error");
    assert_eq!(err.kind, AdapterErrorKind::Execution);
}

#[tokio::test]
async fn down_to_exports_target_env() {
    // The command echoes $FRAISIER_TARGET; the adapter captures it as the log.
    let a = adapter(json!({ "down_to": "printf '%s' \"$FRAISIER_TARGET\"" }));
    let outcome = a
        .down_to(&ctx(), Revision::new("20260101_base"))
        .await
        .expect("down_to");
    assert_eq!(outcome.to, Some(Revision::new("20260101_base")));
    assert_eq!(outcome.log, "20260101_base");
}

#[tokio::test]
async fn verify_pass_fail_and_unconfigured() {
    let pass = adapter(json!({ "verify": "true" }));
    assert!(pass.verify(&ctx()).await.expect("verify").ok);

    let fail = adapter(json!({ "verify": "false" }));
    let report = fail.verify(&ctx()).await.expect("verify ran");
    assert!(!report.ok); // a failing check is a result, not an error
    assert_eq!(report.checks.len(), 1);

    let none = adapter(json!({ "up": "true" }));
    assert!(none.verify(&ctx()).await.expect("vacuous verify").ok);
}

#[tokio::test]
async fn secret_reaches_command_via_env_not_argv() {
    let _guard = ENV_LOCK.lock().await;
    let source = "FRAISIER_CMD_IT_SECRET";
    std::env::set_var(source, "postgres://u:p@h/db");

    let a = adapter(json!({ "current_revision": "printf '%s' \"$DATABASE_URL\"" }));
    let mut c = ctx();
    c.env_secrets
        .insert("DATABASE_URL".to_owned(), source.to_owned());

    let rev = a.current_revision(&c).await.expect("current");
    std::env::remove_var(source);

    // The command read the secret from its environment — proving env injection.
    assert_eq!(rev, Some(Revision::new("postgres://u:p@h/db")));
}

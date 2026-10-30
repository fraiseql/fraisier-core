//! The facts about FraiseQL 2.18.0's `/health` body that a capability check rests
//! on, pinned against real captures (see `fixtures/fraiseql-2.18.0/README.md`).
//!
//! If a FraiseQL release changes any of them, this fails before a check built on
//! the old shape silently starts passing a capability that is not running.

use serde_json::Value;

fn capture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/fraiseql-2.18.0/health-{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn observers(body: &Value) -> &serde_json::Map<String, Value> {
    body["capabilities"]["observers"]
        .as_object()
        .expect("capabilities.observers is an object")
}

#[test]
fn a_running_capability_is_started_and_healthy() {
    let body = capture("running");
    let observers = observers(&body);
    assert_eq!(observers["compiled_in"], Value::Bool(true));
    assert_eq!(observers["started"], Value::Bool(true));
    assert_eq!(observers["healthy"], Value::Bool(true));
    assert_eq!(body["status"], "healthy");
}

#[test]
fn a_running_observer_runtime_does_not_report_serving() {
    // A rule that required `serving` would fail a healthy observer runtime.
    assert!(!observers(&capture("running")).contains_key("serving"));
}

#[test]
fn a_capability_that_failed_to_start_says_so_while_the_body_says_degraded() {
    let body = capture("configured-not-started");
    let observers = observers(&body);
    assert_eq!(observers["started"], Value::Bool(false));
    assert_eq!(observers["healthy"], Value::Bool(false));
    // The server answered 200 with this body (see the fixtures' README), so only
    // the body tells this apart from a running server.
    assert_eq!(body["status"], "degraded");
}

#[test]
fn a_capability_that_is_not_configured_has_no_started_key() {
    let body = capture("not-configured");
    let observers = observers(&body);
    assert_eq!(observers["compiled_in"], Value::Bool(true));
    assert!(!observers.contains_key("started"), "{observers:?}");
    assert!(!observers.contains_key("healthy"), "{observers:?}");
}

#[test]
fn a_capability_not_compiled_in_reports_only_that() {
    let body = capture("not-configured");
    assert_eq!(
        body["capabilities"]["rest"],
        serde_json::json!({ "compiled_in": false })
    );
}

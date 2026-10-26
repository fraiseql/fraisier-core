//! The `require_capabilities` assertion over a FraiseQL `/health` body.

use serde_json::{Map, Value};

/// Check that `body` reports every `required` capability as running.
///
/// A capability runs when `compiled_in && started == Some(true) && healthy !=
/// Some(false)`. `serving` is deliberately ignored: FraiseQL's observer runtime
/// never sets it, even while running.
///
/// # Errors
/// A reason naming every failing capability with the fields it reported, or
/// why the body could not be read at all.
pub fn check(body: &str, required: &[String]) -> Result<(), String> {
    let body: Value =
        serde_json::from_str(body).map_err(|err| format!("health body is not JSON: {err}"))?;
    let reported = body
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| "server does not report capabilities".to_owned())?;
    let failures: Vec<String> = required
        .iter()
        .filter_map(|name| failure(name, reported.get(name)))
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "required capabilities not running: {}",
            failures.join("; ")
        ))
    }
}

/// Why the capability `name` is not running, or `None` when it is.
fn failure(name: &str, entry: Option<&Value>) -> Option<String> {
    let Some(entry) = entry.and_then(Value::as_object) else {
        return Some(format!("server does not report capability '{name}'"));
    };
    if entry.get("compiled_in").and_then(Value::as_bool) != Some(true) {
        return Some(format!("{name}: not compiled in"));
    }
    let started = entry.get("started").and_then(Value::as_bool);
    let healthy = entry.get("healthy").and_then(Value::as_bool);
    match (started, healthy) {
        (Some(true), Some(false)) | (Some(false), _) => Some(format!("{name}: {}", fields(entry))),
        (None, _) => Some(format!("{name}: declared but not configured")),
        (Some(true), _) => None,
    }
}

/// The reported fields of a capability, as `key=value` pairs.
fn fields(entry: &Map<String, Value>) -> String {
    entry
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

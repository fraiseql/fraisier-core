//! `[health].require_capabilities`: the http probe asserts the capabilities a
//! FraiseQL server reports in its `/health` body.
//!
//! A declared capability passes when
//! `compiled_in && started == Some(true) && healthy != Some(false)`. The bodies
//! served here are the real FraiseQL 2.18.0 captures (see
//! `fixtures/fraiseql-2.18.0/README.md`), all of which answered HTTP 200.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use fraisier_adapter_http::HttpHealth;
use fraisier_core::adapter_axes::{AdapterCtx, HealthAdapter, HealthStatus, HostId};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

fn capture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/fraiseql-2.18.0/health-{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// A capture with one field of `capabilities.observers` replaced.
fn capture_with_observers(name: &str, field: &str, value: Value) -> String {
    let mut body: Value = serde_json::from_str(&capture(name)).expect("capture is JSON");
    body["capabilities"]["observers"][field] = value;
    body.to_string()
}

/// Serve `bodies` with HTTP 200, one per request; the last one repeats.
async fn serve(bodies: Vec<String>) -> SocketAddr {
    let queue = Arc::new(Mutex::new(VecDeque::from(bodies)));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let body = {
                let mut queue = queue.lock().expect("queue");
                if queue.len() > 1 {
                    queue.pop_front().unwrap_or_default()
                } else {
                    queue.front().cloned().unwrap_or_default()
                }
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    addr
}

async fn probe(bodies: Vec<String>, required: &[&str], attempts: u32) -> HealthStatus {
    let addr = serve(bodies).await;
    let mut ctx = AdapterCtx::new("checkout", "production");
    ctx.settings
        .insert("url".to_owned(), json!(format!("http://{addr}/health")));
    ctx.settings.insert("attempts".to_owned(), json!(attempts));
    ctx.settings.insert("retry_delay_ms".to_owned(), json!(0));
    ctx.settings
        .insert("require_capabilities".to_owned(), json!(required));
    HttpHealth::new()
        .check(&ctx, &HostId::new("web-1"))
        .await
        .expect("a reachable server is a result, not an error")
}

fn detail(status: &HealthStatus) -> String {
    status.detail.clone().unwrap_or_default()
}

#[tokio::test]
async fn a_running_declared_capability_is_healthy() {
    let status = probe(vec![capture("running")], &["observers"], 1).await;
    assert!(status.healthy, "{}", detail(&status));
}

#[tokio::test]
async fn a_capability_that_failed_to_start_is_unhealthy_despite_http_200() {
    let status = probe(vec![capture("configured-not-started")], &["observers"], 1).await;
    assert!(!status.healthy);
    let detail = detail(&status);
    assert!(detail.contains("observers"), "{detail}");
    assert!(detail.contains("started=false"), "{detail}");
    assert!(detail.contains("healthy=false"), "{detail}");
}

#[tokio::test]
async fn a_declared_capability_that_is_not_configured_is_unhealthy() {
    let status = probe(vec![capture("not-configured")], &["observers"], 1).await;
    assert!(!status.healthy);
    let detail = detail(&status);
    assert!(detail.contains("observers"), "{detail}");
    assert!(detail.contains("declared but not configured"), "{detail}");
}

#[tokio::test]
async fn a_started_capability_reporting_unhealthy_is_unhealthy() {
    let body = capture_with_observers("running", "healthy", json!(false));
    let status = probe(vec![body], &["observers"], 1).await;
    assert!(!status.healthy);
    assert!(
        detail(&status).contains("healthy=false"),
        "{}",
        detail(&status)
    );
}

#[tokio::test]
async fn a_started_capability_that_cannot_tell_its_health_passes() {
    // `healthy` is set only for a capability that can tell; absent is not broken.
    let mut body: Value = serde_json::from_str(&capture("running")).expect("JSON");
    body["capabilities"]["observers"]
        .as_object_mut()
        .expect("object")
        .remove("healthy");
    let status = probe(vec![body.to_string()], &["observers"], 1).await;
    assert!(status.healthy, "{}", detail(&status));
}

#[tokio::test]
async fn a_capability_not_compiled_in_is_unhealthy() {
    let status = probe(vec![capture("running")], &["rest"], 1).await;
    assert!(!status.healthy);
    let detail = detail(&status);
    assert!(detail.contains("rest"), "{detail}");
    assert!(detail.contains("not compiled in"), "{detail}");
}

#[tokio::test]
async fn a_capability_the_server_does_not_report_is_unhealthy() {
    let status = probe(vec![capture("running")], &["teleport"], 1).await;
    assert!(!status.healthy);
    let detail = detail(&status);
    assert!(
        detail.contains("server does not report capability 'teleport'"),
        "{detail}"
    );
}

#[tokio::test]
async fn a_body_without_capabilities_is_unhealthy() {
    // An old server (FraiseQL 2.4.0) reports no capabilities: fail closed.
    let status = probe(
        vec![json!({ "status": "healthy" }).to_string()],
        &["observers"],
        1,
    )
    .await;
    assert!(!status.healthy);
    assert!(
        detail(&status).contains("server does not report capabilities"),
        "{}",
        detail(&status)
    );
}

#[tokio::test]
async fn a_body_that_is_not_json_is_unhealthy() {
    let status = probe(vec!["ok".to_owned()], &["observers"], 1).await;
    assert!(!status.healthy);
    assert!(detail(&status).contains("not JSON"), "{}", detail(&status));
}

#[tokio::test]
async fn every_failing_capability_is_named() {
    let status = probe(
        vec![capture("configured-not-started")],
        &["observers", "rest", "identity"],
        1,
    )
    .await;
    assert!(!status.healthy);
    let detail = detail(&status);
    assert!(detail.contains("observers"), "{detail}");
    assert!(detail.contains("rest"), "{detail}");
    // `identity` is compiled in but not configured, so it fails too.
    assert!(detail.contains("identity"), "{detail}");
}

#[tokio::test]
async fn a_capability_failure_is_retried_like_a_wrong_status() {
    // The boot race: the first probe sees the capability not yet started.
    let status = probe(
        vec![capture("configured-not-started"), capture("running")],
        &["observers"],
        2,
    )
    .await;
    assert!(status.healthy, "{}", detail(&status));
}

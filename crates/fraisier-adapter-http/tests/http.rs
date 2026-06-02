//! Integration tests for the HTTP health adapter.
//!
//! A tiny hand-rolled TCP server returns a fixed status code, so the tests need
//! no external HTTP fixture or extra dependency.

use std::net::SocketAddr;

use fraisier_adapter_http::HttpHealth;
use fraisier_core::adapter_axes::{AdapterCtx, AdapterErrorKind, HealthAdapter, HostId};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// Start a server on an ephemeral port that answers every request with `status`.
async fn start_server(status: u16) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await; // drain the request line/headers
            let body = "ok";
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    addr
}

fn ctx(url: &str, overrides: &serde_json::Value) -> AdapterCtx {
    let mut ctx = AdapterCtx::new("checkout", "production");
    ctx.settings.insert("url".to_owned(), json!(url));
    ctx.settings.insert("attempts".to_owned(), json!(1));
    ctx.settings.insert("retry_delay_ms".to_owned(), json!(0));
    ctx.settings.insert("timeout_ms".to_owned(), json!(1000));
    if let Some(map) = overrides.as_object() {
        for (key, value) in map {
            ctx.settings.insert(key.clone(), value.clone());
        }
    }
    ctx
}

fn host() -> HostId {
    HostId::new("web-1")
}

#[tokio::test]
async fn healthy_when_status_matches() {
    let addr = start_server(200).await;
    let status = HttpHealth::new()
        .check(&ctx(&format!("http://{addr}/health"), &json!({})), &host())
        .await
        .expect("probe");
    assert!(status.healthy);
}

#[tokio::test]
async fn unhealthy_when_status_differs() {
    let addr = start_server(503).await;
    let status = HttpHealth::new()
        .check(&ctx(&format!("http://{addr}/health"), &json!({})), &host())
        .await
        .expect("probe still returns Ok for a reachable-but-unhealthy host");
    assert!(!status.healthy);
    assert!(status.detail.unwrap_or_default().contains("503"));
}

#[tokio::test]
async fn custom_expected_status_is_honoured() {
    let addr = start_server(204).await;
    let status = HttpHealth::new()
        .check(
            &ctx(
                &format!("http://{addr}/health"),
                &json!({ "expected_status": 204 }),
            ),
            &host(),
        )
        .await
        .expect("probe");
    assert!(status.healthy);
}

#[tokio::test]
async fn unreachable_host_is_an_error() {
    // Port 1 has no listener; one short attempt keeps the test fast.
    let err = HttpHealth::new()
        .check(
            &ctx("http://127.0.0.1:1/health", &json!({ "timeout_ms": 300 })),
            &host(),
        )
        .await
        .expect_err("a transport failure is an adapter error, not Ok(unhealthy)");
    assert_eq!(err.kind, AdapterErrorKind::Execution);
    assert_eq!(err.adapter.as_deref(), Some("http"));
}

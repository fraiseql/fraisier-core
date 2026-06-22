//! A minimal HTTP/1.1 receiver for signed webhook POSTs.
//!
//! The surface is deliberately tiny — a webhook endpoint accepts one `POST` per
//! connection, verifies it ([`crate::verify`]), and hands the verified body to a
//! [`WebhookHandler`]. There is no keep-alive, no chunked transfer-encoding, and
//! a hard cap on header and body size, so the parser stays small and auditable.
//! Run it behind a reverse proxy or on a trusted network.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use listenfd::ListenFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::sign::{verify, Rejection, SIGNATURE_HEADER, TIMESTAMP_HEADER};

/// The maximum size of the request line + headers.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// What a verified request triggers. Supplied by the CLI so this crate stays
/// independent of the deploy composition.
#[async_trait]
pub trait WebhookHandler: Send + Sync {
    /// Act on a signature-verified request `body`.
    ///
    /// # Errors
    /// A human-readable message (rendered as an HTTP 500) when the action fails.
    async fn handle(&self, body: &[u8]) -> Result<String, String>;
}

/// Server-side parameters for verifying and bounding requests.
pub struct ServerConfig {
    /// The shared HMAC secret (read from the environment by the caller).
    pub secret: Vec<u8>,
    /// The replay tolerance, in seconds, in either direction.
    pub tolerance_secs: u64,
    /// The maximum accepted request body size, in bytes.
    pub max_body_bytes: usize,
    /// How long to wait for a complete request before giving up.
    pub read_timeout: Duration,
    /// Self-upgrade drain behaviour: refuse new deploys with a retriable 503 while
    /// a coordinated restart is settling in-flight deploys.
    pub drain: Drain,
}

/// The self-upgrade drain state, shared with the restart coordinator via a file.
///
/// The two run as separate processes, so the flag is a **file**. While it exists,
/// a verified deploy POST is refused with `503` + `Retry-After` so upstream callers
/// (CI, monitors) record a loud, retriable failure instead of a dropped request.
#[derive(Debug, Clone, Default)]
pub struct Drain {
    /// The drain-flag file. The server is draining iff it is set and exists.
    /// `None` (the default) disables draining entirely.
    pub flag_path: Option<PathBuf>,
    /// The `Retry-After` value (seconds) sent on a drain refusal.
    pub retry_after_s: u64,
    /// The fraise names reported in the refusal body (informational, for callers).
    pub refused: Vec<String>,
}

impl Drain {
    /// Whether the server is currently draining (the flag file exists).
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.flag_path.as_ref().is_some_and(|path| path.exists())
    }

    /// The JSON refusal body: `{status, retry_after_s, refused}`.
    fn refusal_body(&self) -> String {
        let refused = self
            .refused
            .iter()
            .map(|fraise| format!("{fraise:?}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"status\":\"draining\",\"retry_after_s\":{},\"refused\":[{refused}]}}",
            self.retry_after_s
        )
    }
}

/// The outcome of serving one connection (returned for the accept loop / tests).
#[derive(Debug, PartialEq, Eq)]
pub enum Served {
    /// The request verified and the handler succeeded.
    Ok,
    /// A `GET /healthz` liveness probe was answered 200 (no HMAC, no handler).
    Health,
    /// The request failed signature/replay verification (HTTP 401).
    Rejected(Rejection),
    /// The request was malformed, too large, timed out, or not a POST.
    BadRequest(String),
    /// The handler returned an error (HTTP 500).
    HandlerError(String),
    /// A verified deploy was refused with a retriable `503` because the server is
    /// draining for a self-upgrade restart.
    Draining,
}

/// A parsed HTTP request.
///
/// Header names are lower-cased on parse for case-insensitive lookup.
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Serve exactly one request on `stream`.
///
/// Verifies it against `config` at `now` (Unix seconds) and dispatches a
/// verified body to `handler`. Always writes an HTTP response; the returned
/// [`Served`] reports what happened.
pub async fn serve_connection<S>(
    mut stream: S,
    config: &ServerConfig,
    handler: &dyn WebhookHandler,
    now: u64,
) -> Served
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = match tokio::time::timeout(
        config.read_timeout,
        read_request(&mut stream, config.max_body_bytes),
    )
    .await
    {
        Ok(Ok(request)) => request,
        Ok(Err(message)) => {
            let _ = respond(&mut stream, 400, "Bad Request", &message).await;
            return Served::BadRequest(message);
        }
        Err(_) => {
            let message = "request read timed out".to_owned();
            let _ = respond(&mut stream, 408, "Request Timeout", &message).await;
            return Served::BadRequest(message);
        }
    };

    // Liveness probe: a `GET /healthz` is answered 200 the moment we are accepting
    // connections, *before* the HMAC gate — it is the only unauthenticated route, by
    // design (it proves the server is up, nothing more). The body is bare "ok": it
    // must leak no version/build detail. apply (and blue-green's HealthGateGreen) read
    // this signal out-of-process; the deploy handler is never invoked.
    if request.method.eq_ignore_ascii_case("GET") && request.path == "/healthz" {
        let _ = respond(&mut stream, 200, "OK", "ok").await;
        return Served::Health;
    }

    if !request.method.eq_ignore_ascii_case("POST") {
        let _ = respond(&mut stream, 405, "Method Not Allowed", "POST required").await;
        return Served::BadRequest(format!("method {} not allowed", request.method));
    }

    if let Err(rejection) = verify(
        &config.secret,
        &request.body,
        request.header(TIMESTAMP_HEADER),
        request.header(SIGNATURE_HEADER),
        now,
        config.tolerance_secs,
    ) {
        let _ = respond(&mut stream, 401, "Unauthorized", &rejection.to_string()).await;
        return Served::Rejected(rejection);
    }

    // The request is authentic; if a self-upgrade restart is draining, refuse it
    // with a retriable 503 rather than dropping it mid-restart.
    if config.drain.is_draining() {
        let _ = respond_draining(&mut stream, &config.drain).await;
        return Served::Draining;
    }

    match handler.handle(&request.body).await {
        Ok(note) => {
            let _ = respond(&mut stream, 200, "OK", &note).await;
            Served::Ok
        }
        Err(message) => {
            let _ = respond(&mut stream, 500, "Internal Server Error", &message).await;
            Served::HandlerError(message)
        }
    }
}

/// Where the listening socket came from.
#[derive(Debug, PartialEq, Eq)]
pub enum ListenSource {
    /// Inherited from systemd via socket activation (`LISTEN_FDS`).
    SocketActivated,
    /// Bound directly to the given address (the standalone fallback).
    Bound(String),
}

impl std::fmt::Display for ListenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SocketActivated => write!(f, "socket activation (systemd)"),
            Self::Bound(addr) => write!(f, "{addr}"),
        }
    }
}

/// Acquire the listening socket: systemd **socket activation** when a socket was
/// passed in (`LISTEN_FDS`), otherwise **bind** `addr` directly.
///
/// The raw-fd handling for socket activation lives inside the `listenfd` crate,
/// so this crate stays free of `unsafe`.
///
/// # Errors
/// [`std::io::Error`] if the inherited socket cannot be adopted or `addr` cannot
/// be bound.
pub async fn acquire(addr: &str) -> std::io::Result<(TcpListener, ListenSource)> {
    if let Some(std_listener) = ListenFd::from_env().take_tcp_listener(0).ok().flatten() {
        std_listener.set_nonblocking(true)?;
        return Ok((
            TcpListener::from_std(std_listener)?,
            ListenSource::SocketActivated,
        ));
    }
    Ok((
        TcpListener::bind(addr).await?,
        ListenSource::Bound(addr.to_owned()),
    ))
}

/// Accept connections, serving each with [`serve_connection`], until `SIGTERM`.
///
/// Connections are handled one at a time: webhook volume is low and deploys
/// serialize on the state-store lock anyway, so sequential handling keeps the
/// server simple and avoids unbounded concurrent deploys.
///
/// On `SIGTERM` it shuts down **gracefully**: an in-flight request runs to
/// completion (it is awaited outside the accept/signal select), then `serve`
/// returns `Ok(())`. This is what makes `systemctl restart` a *coordinated*
/// restart — the request being processed is not cut off mid-deploy.
///
/// # Errors
/// [`std::io::Error`] from installing the signal handler or from
/// [`TcpListener::accept`].
pub async fn serve(
    listener: TcpListener,
    config: &ServerConfig,
    handler: &dyn WebhookHandler,
) -> std::io::Result<()> {
    let mut shutdown = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _peer) = accepted?;
                // Awaited here, not inside the select, so a SIGTERM that arrives
                // mid-request is latched and observed only after it completes.
                let _ = serve_connection(stream, config, handler, now_unix()).await;
            }
            _ = shutdown.recv() => return Ok(()),
        }
    }
}

/// The current time in Unix seconds (saturating to 0 before the epoch).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Read one HTTP/1.1 request: the request line, headers (until `\r\n\r\n`), and a
/// `Content-Length`-delimited body. Caps header and body size.
async fn read_request<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_body: usize,
) -> Result<Request, String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err("request headers too large".to_owned());
        }
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|error| format!("read error: {error}"))?;
        if read == 0 {
            return Err("connection closed before the headers completed".to_owned());
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let head =
        std::str::from_utf8(&buf[..header_end]).map_err(|_| "non-UTF-8 headers".to_owned())?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or_else(|| "empty request".to_owned())?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "missing request method".to_owned())?
        .to_owned();
    // The path gates only the unauthenticated `GET /healthz` liveness route; for a
    // signed POST the HMAC, not the path, is the authorization.
    let path = parts.next().unwrap_or("/").to_owned();

    let mut headers = Vec::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.push((key.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }

    let content_length: usize = headers
        .iter()
        .find(|(key, _)| key == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    if content_length > max_body {
        return Err(format!(
            "request body too large ({content_length} > {max_body} bytes)"
        ));
    }

    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|error| format!("read error: {error}"))?;
        if read == 0 {
            break;
        }
        let take = (content_length - body.len()).min(read);
        body.extend_from_slice(&chunk[..take]);
    }
    body.truncate(content_length);

    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

/// Write a minimal `Connection: close` HTTP response with a plain-text body.
async fn respond<S: AsyncWrite + Unpin>(
    stream: &mut S,
    code: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let payload = format!("{body}\n");
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(payload.as_bytes()).await?;
    stream.flush().await
}

/// Write the `503 Service Unavailable` drain refusal: a `Retry-After` header and
/// a JSON body naming the refused fraises.
async fn respond_draining<S: AsyncWrite + Unpin>(
    stream: &mut S,
    drain: &Drain,
) -> std::io::Result<()> {
    let payload = format!("{}\n", drain.refusal_body());
    let head = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
         Retry-After: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        drain.retry_after_s,
        payload.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(payload.as_bytes()).await?;
    stream.flush().await
}

/// The index of the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::{serve_connection, Drain, Served, ServerConfig, WebhookHandler};
    use crate::sign::sign;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    const SECRET: &[u8] = b"hook-secret";
    const NOW: u64 = 1_700_000_000;

    /// A handler that records how many bodies it was given.
    struct Recorder {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl WebhookHandler for Recorder {
        async fn handle(&self, _body: &[u8]) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok("deployed".to_owned())
        }
    }

    fn config() -> ServerConfig {
        ServerConfig {
            secret: SECRET.to_vec(),
            tolerance_secs: 300,
            max_body_bytes: 1024,
            read_timeout: Duration::from_secs(5),
            drain: Drain::default(),
        }
    }

    /// Build a raw HTTP/1.1 POST with the given (timestamp, signature) headers.
    fn raw_post(body: &[u8], timestamp: &str, signature: &str) -> Vec<u8> {
        let mut request = format!(
            "POST /deploy HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\
             x-fraisier-timestamp: {timestamp}\r\nx-fraisier-signature: {signature}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        request
    }

    /// Drive one request through a duplex pipe, returning (outcome, response text).
    async fn serve_once(request: &[u8], calls: Arc<AtomicUsize>) -> (Served, String) {
        let (client, server) = tokio::io::duplex(8192);
        let handler = Recorder { calls };
        let cfg = config();
        let write = {
            use tokio::io::AsyncWriteExt;
            let request = request.to_vec();
            tokio::spawn(async move {
                let mut client_w = client;
                client_w.write_all(&request).await.expect("write request");
                let mut response = String::new();
                client_w.read_to_string(&mut response).await.expect("read");
                response
            })
        };
        let outcome = serve_connection(server, &cfg, &handler, NOW).await;
        let response = write.await.expect("client task");
        (outcome, response)
    }

    #[tokio::test]
    async fn a_valid_signed_post_invokes_the_handler() {
        let body = br#"{"version":"1.2.3"}"#;
        let request = raw_post(body, &NOW.to_string(), &sign(SECRET, NOW, body));
        let calls = Arc::new(AtomicUsize::new(0));
        let (outcome, response) = serve_once(&request, calls.clone()).await;
        assert_eq!(outcome, Served::Ok);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "handler ran once");
        assert!(response.starts_with("HTTP/1.1 200"), "response: {response}");
    }

    #[tokio::test]
    async fn a_tampered_body_is_rejected_and_the_handler_never_runs() {
        let signed = br#"{"version":"1.2.3"}"#;
        let tampered = br#"{"version":"9.9.9"}"#;
        let request = raw_post(tampered, &NOW.to_string(), &sign(SECRET, NOW, signed));
        let calls = Arc::new(AtomicUsize::new(0));
        let (outcome, response) = serve_once(&request, calls.clone()).await;
        assert!(matches!(outcome, Served::Rejected(_)), "{outcome:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "handler must not run");
        assert!(response.starts_with("HTTP/1.1 401"), "response: {response}");
    }

    #[tokio::test]
    async fn a_non_post_is_refused() {
        let calls = Arc::new(AtomicUsize::new(0));
        let request = b"GET /deploy HTTP/1.1\r\nHost: x\r\n\r\n";
        let (outcome, response) = serve_once(request, calls.clone()).await;
        assert!(matches!(outcome, Served::BadRequest(_)), "{outcome:?}");
        assert!(response.starts_with("HTTP/1.1 405"), "response: {response}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_get_healthz_is_a_200_liveness_probe_with_no_secret() {
        // Liveness: reaching serve_connection means the accept loop is up, so
        // `GET /healthz` answers 200 *without* an HMAC (the only non-signed route).
        // The handler (a deploy) must never run, and the body leaks nothing beyond
        // "up" — no version/build detail (the route is unauthenticated by design).
        let calls = Arc::new(AtomicUsize::new(0));
        let request = b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n";
        let (outcome, response) = serve_once(request, calls.clone()).await;
        assert_eq!(outcome, Served::Health, "{outcome:?}");
        assert!(response.starts_with("HTTP/1.1 200"), "response: {response}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no deploy on a probe");
        let body = response.rsplit("\r\n\r\n").next().unwrap_or_default();
        assert_eq!(body, "ok\n", "body is bare liveness, no version/build");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serves_a_signed_post_over_a_real_tcp_socket() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let handler = Recorder {
                calls: server_calls,
            };
            serve_connection(stream, &config(), &handler, NOW).await
        });

        let body = br#"{"version":"2.0.0"}"#;
        let request = raw_post(body, &NOW.to_string(), &sign(SECRET, NOW, body));
        let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
        client.write_all(&request).await.expect("write");
        let mut response = String::new();
        client.read_to_string(&mut response).await.expect("read");

        let outcome = server.await.expect("server task");
        assert_eq!(outcome, Served::Ok);
        assert!(response.starts_with("HTTP/1.1 200"), "response: {response}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "handler ran once");
    }

    #[tokio::test]
    async fn a_draining_server_refuses_a_verified_deploy_with_503() {
        use tokio::io::AsyncWriteExt;
        let flag = std::env::temp_dir().join(format!("fraisier-drain-{}.flag", std::process::id()));
        std::fs::write(&flag, b"").expect("write drain flag");
        let cfg = ServerConfig {
            secret: SECRET.to_vec(),
            tolerance_secs: 300,
            max_body_bytes: 1024,
            read_timeout: Duration::from_secs(5),
            drain: Drain {
                flag_path: Some(flag.clone()),
                retry_after_s: 42,
                refused: vec!["checkout".to_owned()],
            },
        };
        let body = br#"{"version":"1.2.3"}"#;
        let request = raw_post(body, &NOW.to_string(), &sign(SECRET, NOW, body));
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = Recorder {
            calls: calls.clone(),
        };
        let (client, server) = tokio::io::duplex(8192);
        let write = tokio::spawn(async move {
            let mut client_w = client;
            client_w.write_all(&request).await.expect("write");
            let mut response = String::new();
            client_w.read_to_string(&mut response).await.expect("read");
            response
        });
        let outcome = serve_connection(server, &cfg, &handler, NOW).await;
        let response = write.await.expect("client task");
        let _ = std::fs::remove_file(&flag);

        assert_eq!(outcome, Served::Draining, "{outcome:?}");
        assert!(response.starts_with("HTTP/1.1 503"), "response: {response}");
        assert!(response.contains("Retry-After: 42"), "response: {response}");
        assert!(
            response.contains("\"status\":\"draining\"") && response.contains("\"checkout\""),
            "drain body names the refused fraise: {response}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no deploy runs while draining"
        );
    }

    #[tokio::test]
    async fn a_non_draining_server_serves_normally() {
        // The same request, with no drain flag set, deploys as usual.
        let body = br#"{"version":"1.2.3"}"#;
        let request = raw_post(body, &NOW.to_string(), &sign(SECRET, NOW, body));
        let calls = Arc::new(AtomicUsize::new(0));
        let (outcome, response) = serve_once(&request, calls.clone()).await;
        assert_eq!(outcome, Served::Ok);
        assert!(response.starts_with("HTTP/1.1 200"), "response: {response}");
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused() {
        // Declare a Content-Length beyond max_body_bytes (1024).
        let request = b"POST /deploy HTTP/1.1\r\nContent-Length: 99999\r\n\r\n";
        let calls = Arc::new(AtomicUsize::new(0));
        let (outcome, response) = serve_once(request, calls.clone()).await;
        assert!(matches!(outcome, Served::BadRequest(_)), "{outcome:?}");
        assert!(response.starts_with("HTTP/1.1 400"), "response: {response}");
    }
}

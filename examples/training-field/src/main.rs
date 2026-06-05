//! fraisier training-field app — a deliberately tiny, DB-backed service used to
//! exercise the *real* fraisier deploy pipeline end-to-end with the in-process
//! Confiture migration adapter against a real Postgres (see
//! `scripts/checkpoint-training.sh`). It is NOT shipped — it is test scaffolding,
//! the Confiture-flavoured sibling of the synthetic sqlx matrix.
//!
//! Behaviour is a function of the active release's version name (read off the
//! `current` symlink fraisier activates, exactly as a real app reads its build):
//!   * a name containing `crash` exits before readiness → `systemctl restart`
//!     fails (the release/restart phase);
//!   * a name containing `sick` serves HTTP 500 → the health probe fails;
//!   * otherwise it serves 200 *iff* the Confiture-migrated `notes` table is
//!     queryable — so a deploy is only healthy once migrate actually applied the
//!     schema this build expects.
//!
//! Readiness is signalled via `sd_notify` (`Type=notify`), so a start only
//! succeeds once the listener is up and the database reachable.

use std::env;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let active = env::args().nth(1).unwrap_or_default();
    let port: u16 = env::args()
        .nth(2)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(8080);
    let version = active_version(&active);

    // Release-phase failure: a "crash" build never reaches readiness, so the
    // saga's `systemctl restart` of this release fails and it rolls back.
    if version.contains("crash") {
        eprintln!("training-app: crash build '{version}' — exiting before readiness");
        std::process::exit(1);
    }

    let dsn = env::var("TRAINING_DATABASE_URL").map_err(|_| "TRAINING_DATABASE_URL must be set")?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&dsn)
        .await?;

    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    sd_notify::notify(false, &[sd_notify::NotifyState::Ready])?;
    eprintln!("training-app: '{version}' listening on :{port}");

    loop {
        let (stream, _) = listener.accept().await?;
        let pool = pool.clone();
        let version = version.clone();
        tokio::spawn(async move {
            if let Err(err) = serve(stream, &pool, &version).await {
                eprintln!("training-app: connection error: {err}");
            }
        });
    }
}

/// The active release's version: the basename of the `current` symlink target,
/// with the `app-` prefix and `.tar.gz` suffix stripped (matching how the
/// checkpoint mints releases). `"none"` when nothing is activated yet.
fn active_version(active: &str) -> String {
    std::fs::read_link(active)
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .map_or_else(
            || "none".to_owned(),
            |name| {
                name.trim_start_matches("app-")
                    .trim_end_matches(".tar.gz")
                    .to_owned()
            },
        )
}

/// Whether this build is healthy: not a `sick` build, and the migrated `notes`
/// table is queryable (proving Confiture applied the schema this build needs).
async fn is_healthy(pool: &PgPool, version: &str) -> bool {
    if version.contains("sick") {
        return false;
    }
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notes")
        .fetch_one(pool)
        .await
        .is_ok()
}

/// Serve one request: drain it (every path is the health probe) and reply
/// 200/500 per [`is_healthy`].
async fn serve(mut stream: TcpStream, pool: &PgPool, version: &str) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf).await?;
    let (status, body) = if is_healthy(pool, version).await {
        ("200 OK", version)
    } else {
        ("500 Internal Server Error", "unhealthy")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await
}

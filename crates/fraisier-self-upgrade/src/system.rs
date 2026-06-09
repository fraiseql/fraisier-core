//! Production implementations of the apply engine's out-of-process handles.
//!
//! A [`SystemctlSupervisor`] (drives `systemctl`) and an [`HttpHealth`] probe
//! (HTTP `GET /healthz`). These are the only pieces that touch the live system,
//! and — per the load-bearing invariant — **neither ever `exec`s the swapped
//! binary**: restart goes through the supervisor, readiness through HTTP.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

use crate::apply::{Health, Supervisor};

/// A [`Supervisor`] backed by `systemctl [--user] <verb> <unit>`.
#[derive(Debug, Clone)]
pub struct SystemctlSupervisor {
    unit: String,
    user: bool,
}

impl SystemctlSupervisor {
    /// Drive `unit`, optionally via the per-user systemd manager.
    #[must_use]
    pub fn new(unit: impl Into<String>, user: bool) -> Self {
        Self {
            unit: unit.into(),
            user,
        }
    }

    /// Build `systemctl [--user] <verb> <unit>` as a `std::process::Command`
    /// (separated out, and std-typed, so its argv is testable without a live
    /// systemd manager; converted to a tokio command only to run it).
    fn command(&self, verb: &str) -> std::process::Command {
        let mut command = std::process::Command::new("systemctl");
        if self.user {
            command.arg("--user");
        }
        command.arg(verb).arg(&self.unit);
        command
    }
}

#[async_trait]
impl Supervisor for SystemctlSupervisor {
    async fn restart(&self) -> Result<(), String> {
        let output = tokio::process::Command::from(self.command("restart"))
            .output()
            .await
            .map_err(|cause| format!("spawning `systemctl restart {}`: {cause}", self.unit))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }

    async fn is_active(&self) -> bool {
        tokio::process::Command::from(self.command("is-active"))
            .output()
            .await
            .is_ok_and(|output| output.status.success())
    }
}

/// A [`Health`] probe that GETs an HTTP `/healthz` URL and treats a 2xx as up.
#[derive(Debug, Clone)]
pub struct HttpHealth {
    url: String,
    timeout: Duration,
}

impl HttpHealth {
    /// Probe `url` with a per-request `timeout`.
    #[must_use]
    pub fn new(url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            url: url.into(),
            timeout,
        }
    }
}

#[async_trait]
impl Health for HttpHealth {
    async fn healthy(&self) -> bool {
        let Ok(client) = reqwest::Client::builder().timeout(self.timeout).build() else {
            return false;
        };
        client
            .get(&self.url)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }
}

/// Whether this looks like a systemd-managed deployment.
///
/// This is the precondition for a safe apply (the auto-revert is
/// systemd-managed). For the system manager it is the canonical `sd_booted()`
/// check (`/run/systemd/system` exists); for the per-user manager, a `systemd`
/// directory under `$XDG_RUNTIME_DIR`.
#[must_use]
pub fn systemd_available(user: bool) -> bool {
    if user {
        std::env::var_os("XDG_RUNTIME_DIR")
            .is_some_and(|dir| Path::new(&dir).join("systemd").exists())
    } else {
        booted_at(Path::new("/run/systemd/system"))
    }
}

/// `sd_booted()` reduced to a path existence check (factored out so the policy is
/// testable against a temp marker rather than the host's real `/run`).
fn booted_at(marker: &Path) -> bool {
    marker.exists()
}

#[cfg(test)]
mod tests {
    use super::{booted_at, HttpHealth, SystemctlSupervisor};
    use crate::apply::Health as _;
    use std::time::Duration;

    #[test]
    fn systemctl_command_is_built_correctly() {
        let system = SystemctlSupervisor::new("fraisier-webhook.service", false).command("restart");
        assert_eq!(system.get_program().to_string_lossy(), "systemctl");
        let args: Vec<_> = system
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["restart", "fraisier-webhook.service"]);

        let user = SystemctlSupervisor::new("u.service", true).command("is-active");
        let args: Vec<_> = user
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["--user", "is-active", "u.service"]);
    }

    #[test]
    fn booted_at_reflects_the_marker_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("run-systemd-system");
        assert!(!booted_at(&marker), "absent marker -> not booted");
        std::fs::create_dir(&marker).expect("create marker");
        assert!(booted_at(&marker), "present marker -> booted");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_health_is_true_for_200_and_false_for_a_dead_port() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .expect("write");
            let _ = stream.flush().await;
        });

        let healthy = HttpHealth::new(format!("http://{addr}/healthz"), Duration::from_secs(2));
        assert!(healthy.healthy().await, "200 -> healthy");
        server.await.expect("server task");

        // Nothing is listening here now -> connection refused -> not healthy.
        let dead = HttpHealth::new(format!("http://{addr}/healthz"), Duration::from_millis(300));
        assert!(!dead.healthy().await, "dead port -> not healthy");
    }
}

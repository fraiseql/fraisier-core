//! # fraisier-adapter-docker-compose
//!
//! The [`DockerComposeService`] adapter: a [`ServiceAdapter`] that restarts and
//! reports a Compose service through the `docker compose` CLI (PRD §6.3).
//!
//! ## Configuration
//!
//! Read per call from [`AdapterCtx::settings`] (the `[service]` table):
//!
//! ```toml
//! [service]
//! adapter = "docker-compose"
//! compose_service = "web"                 # the service within the project (required)
//! compose_file = "/srv/app/compose.yaml"  # optional; the Compose default is used if omitted
//! ```
//!
//! ## v2 vs v1
//!
//! Compose ships in two shapes: the v2 `docker compose …` subcommand and the
//! legacy v1 `docker-compose …` standalone binary. The adapter spawns `docker`
//! and prepends `compose` by default; point [`DockerComposeService::with_program`]
//! (or `FRAISIER_DOCKER_BIN`) at a `docker-compose` binary and it drops the
//! subcommand automatically (inferred from the program's basename).
//!
//! ## Status
//!
//! `status` runs `… ps --format json <service>` and reads the per-container
//! `State`/`Status` fields, tolerating both an NDJSON stream and a JSON array
//! (Compose has emitted both across versions), and falling back to the legacy
//! plain-text table. A stopped service is a normal `running: false` result.
//!
//! ## Locality
//!
//! Phase 1/2 run `docker` on the **local** host; the `host` argument is reserved
//! for the Phase 3+ SSH dispatch layer.

use std::ffi::OsString;
use std::path::Path;

use async_trait::async_trait;
use fraisier_adapter_support::{error, run_command, Captured};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterError, AdapterErrorKind, HostId, ServiceAdapter, ServiceStatus,
};
use serde_json::{Map, Value};

/// The adapter's identity name.
const ADAPTER_NAME: &str = "docker-compose";

/// The env var that overrides which `docker` binary the adapter spawns.
const PROGRAM_ENV: &str = "FRAISIER_DOCKER_BIN";

/// A [`ServiceAdapter`] backed by the Docker Compose CLI.
///
/// # Example
/// ```
/// use fraisier_adapter_docker_compose::DockerComposeService;
///
/// let v2 = DockerComposeService::new(); // spawns `docker compose …`
/// let v1 = DockerComposeService::with_program("/usr/local/bin/docker-compose");
/// let _ = (v2, v1);
/// ```
pub struct DockerComposeService {
    program: OsString,
    /// Whether to prepend the `compose` subcommand (v2). `false` for the v1
    /// `docker-compose` standalone binary.
    compose_subcommand: bool,
}

impl Default for DockerComposeService {
    fn default() -> Self {
        Self::new()
    }
}

impl DockerComposeService {
    /// Create an adapter that spawns `docker compose` (honouring the
    /// `FRAISIER_DOCKER_BIN` override, whose basename selects v1 vs v2).
    #[must_use]
    pub fn new() -> Self {
        match std::env::var_os(PROGRAM_ENV) {
            Some(program) if !program.is_empty() => Self::with_program(program),
            _ => Self {
                program: OsString::from("docker"),
                compose_subcommand: true,
            },
        }
    }

    /// Create an adapter that spawns the binary at `program`. A program whose
    /// basename contains `docker-compose` is treated as the v1 standalone binary
    /// (no `compose` subcommand); anything else is treated as v2 `docker`.
    #[must_use]
    pub fn with_program(program: impl Into<OsString>) -> Self {
        let program = program.into();
        let is_v1 = Path::new(&program)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("docker-compose"));
        Self {
            program,
            compose_subcommand: !is_v1,
        }
    }

    /// Build the argv for a Compose invocation:
    /// `[compose] [-f <file>] <tail…> <service>`.
    fn compose_args(
        ctx: &AdapterCtx,
        compose_subcommand: bool,
        tail: &[&str],
        operation: &str,
    ) -> Result<Vec<OsString>, AdapterError> {
        let service = setting(ctx, "compose_service").ok_or_else(|| {
            error(
                AdapterErrorKind::InvalidConfig,
                ADAPTER_NAME,
                operation,
                "no 'compose_service' configured in [service] settings".to_owned(),
                None,
            )
        })?;
        let mut args = Vec::new();
        if compose_subcommand {
            args.push(OsString::from("compose"));
        }
        if let Some(file) = setting(ctx, "compose_file") {
            args.push(OsString::from("-f"));
            args.push(OsString::from(file));
        }
        args.extend(tail.iter().map(OsString::from));
        args.push(OsString::from(service));
        Ok(args)
    }

    /// Run a Compose invocation, returning the captured output.
    async fn compose(
        &self,
        ctx: &AdapterCtx,
        tail: &[&str],
        operation: &str,
    ) -> Result<Captured, AdapterError> {
        let args = Self::compose_args(ctx, self.compose_subcommand, tail, operation)?;
        run_command(&self.program, &args, &[], None, ADAPTER_NAME, operation).await
    }
}

/// Read a non-empty string setting from the `[service]` table.
fn setting<'a>(ctx: &'a AdapterCtx, key: &str) -> Option<&'a str> {
    ctx.settings
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// Interpret a `docker compose ps --format json <service>` result.
fn parse_status(captured: &Captured) -> ServiceStatus {
    let text = captured.stdout.trim();
    let mut running = false;
    let mut detail = None;
    for container in parse_containers(text) {
        let state = container.get("State").and_then(Value::as_str);
        let status = container.get("Status").and_then(Value::as_str);
        if let Some(shown) = state.or(status) {
            detail = Some(shown.to_owned());
        }
        if state.is_some_and(|s| s.eq_ignore_ascii_case("running"))
            || status.is_some_and(|s| s.starts_with("Up"))
        {
            running = true;
        }
    }
    // Legacy plain-text `docker-compose ps` table, when no JSON was parsed.
    if detail.is_none() && !text.is_empty() {
        detail = text.lines().next_back().map(str::trim).map(str::to_owned);
        running = text.contains("Up");
    }
    ServiceStatus { running, detail }
}

/// Parse Compose `ps --format json` output into container objects, tolerating a
/// JSON array, a single object, or an NDJSON stream.
fn parse_containers(text: &str) -> Vec<Map<String, Value>> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return match value {
            Value::Array(items) => items.into_iter().filter_map(into_object).collect(),
            Value::Object(map) => vec![map],
            _ => Vec::new(),
        };
    }
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(into_object)
        .collect()
}

fn into_object(value: Value) -> Option<Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

#[async_trait]
impl ServiceAdapter for DockerComposeService {
    async fn restart(&self, ctx: &AdapterCtx, _host: &HostId) -> Result<(), AdapterError> {
        let captured = self.compose(ctx, &["restart"], "restart").await?;
        if captured.succeeded() {
            return Ok(());
        }
        let code = captured
            .code
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        Err(error(
            AdapterErrorKind::Execution,
            ADAPTER_NAME,
            "restart",
            format!("`docker compose restart` exited with {code}"),
            captured.stderr_opt(),
        ))
    }

    async fn status(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
    ) -> Result<ServiceStatus, AdapterError> {
        // A stopped service still exits 0 from `ps`; only a spawn failure errors.
        let captured = self
            .compose(ctx, &["ps", "--format", "json"], "status")
            .await?;
        Ok(parse_status(&captured))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_status, DockerComposeService};
    use fraisier_adapter_support::Captured;
    use fraisier_core::adapter_axes::AdapterCtx;
    use serde_json::json;

    fn ctx_with(pairs: &[(&str, &str)]) -> AdapterCtx {
        let mut ctx = AdapterCtx::new("checkout", "production");
        for (key, value) in pairs {
            ctx.settings.insert((*key).to_owned(), json!(value));
        }
        ctx
    }

    fn stdout(text: &str) -> Captured {
        Captured {
            code: Some(0),
            stdout: text.to_owned(),
            stderr: String::new(),
        }
    }

    #[test]
    fn restart_args_require_a_compose_service() {
        let ctx = ctx_with(&[]);
        let err = DockerComposeService::compose_args(&ctx, true, &["restart"], "restart")
            .expect_err("missing compose_service must fail");
        assert_eq!(err.adapter.as_deref(), Some("docker-compose"));
    }

    #[test]
    fn v2_restart_args_prepend_the_compose_subcommand_and_file() {
        let ctx = ctx_with(&[("compose_service", "web"), ("compose_file", "compose.yaml")]);
        let args =
            DockerComposeService::compose_args(&ctx, true, &["restart"], "restart").expect("args");
        assert_eq!(
            args,
            vec!["compose", "-f", "compose.yaml", "restart", "web"]
        );
    }

    #[test]
    fn v1_restart_args_omit_the_compose_subcommand() {
        let ctx = ctx_with(&[("compose_service", "web")]);
        let args =
            DockerComposeService::compose_args(&ctx, false, &["restart"], "restart").expect("args");
        assert_eq!(args, vec!["restart", "web"]);
    }

    #[test]
    fn status_args_request_json_for_the_service() {
        let ctx = ctx_with(&[("compose_service", "web")]);
        let args =
            DockerComposeService::compose_args(&ctx, true, &["ps", "--format", "json"], "status")
                .expect("args");
        assert_eq!(args, vec!["compose", "ps", "--format", "json", "web"]);
    }

    #[test]
    fn with_program_infers_v1_from_the_basename() {
        let v1 = DockerComposeService::with_program("/usr/local/bin/docker-compose");
        assert!(!v1.compose_subcommand);
        let v2 = DockerComposeService::with_program("/usr/bin/docker");
        assert!(v2.compose_subcommand);
    }

    #[test]
    fn parse_status_reads_ndjson_state() {
        let status = parse_status(&stdout(
            r#"{"Service":"web","State":"running","Status":"Up 3 minutes"}"#,
        ));
        assert!(status.running);
        assert_eq!(status.detail.as_deref(), Some("running"));
    }

    #[test]
    fn parse_status_reads_a_json_array() {
        let status = parse_status(&stdout(
            r#"[{"Service":"web","State":"exited","Status":"Exited (0)"}]"#,
        ));
        assert!(!status.running);
        assert_eq!(status.detail.as_deref(), Some("exited"));
    }

    #[test]
    fn parse_status_falls_back_to_plain_text() {
        // Legacy `docker-compose ps` table output (no JSON).
        let status = parse_status(&stdout(
            "Name        Command   State    Ports\n----\napp_web_1   start     Up       80/tcp",
        ));
        assert!(status.running);
        assert!(status.detail.is_some());
    }
}

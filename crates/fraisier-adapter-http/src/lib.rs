//! # fraisier-adapter-http
//!
//! The [`HttpHealth`] adapter: a [`HealthAdapter`] that probes a host with an
//! HTTP `GET` and retries until it is healthy or the attempts are exhausted.
//!
//! ## Configuration
//!
//! Read per call from [`AdapterCtx::settings`] (the `[health]` table):
//!
//! ```toml
//! [health]
//! adapter = "http"
//! url = "http://{host.address}:8080/health"   # `{host.address}` → the host's
//!                                              # inventory address; `{host}` → its id
//! expected_status = 200                   # default 200
//! attempts = 3                            # default 3
//! retry_delay_ms = 500                    # default 500
//! timeout_ms = 5000                       # default 5000
//! ```
//!
//! ## Healthy vs. unreachable
//!
//! The trait distinguishes a probe *result* from a probe *failure*:
//! - a response whose status matches `expected_status` → `Ok(healthy: true)`;
//! - a response with any other status → `Ok(healthy: false)` (a result);
//! - a transport failure (refused, timeout, DNS) after every attempt →
//!   `Err(..)` (the probe could not be performed).

use std::time::Duration;

use async_trait::async_trait;
use fraisier_adapter_support::{error, retry_on_err};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterError, AdapterErrorKind, HealthAdapter, HealthStatus, HostId,
};
use fraisier_core::token_provider::TokenProvider;
use serde_json::Value;
use tokio::sync::OnceCell;

mod token;

/// The adapter's identity name.
const ADAPTER_NAME: &str = "http";

const DEFAULT_EXPECTED_STATUS: u16 = 200;
const DEFAULT_ATTEMPTS: u32 = 3;
const DEFAULT_RETRY_DELAY_MS: u64 = 500;
const DEFAULT_TIMEOUT_MS: u64 = 5000;

/// Why a single probe did not report healthy.
enum ProbeFailure {
    /// A response arrived, but its status was not the expected one.
    Unhealthy(u16),
    /// The request could not be completed (refused, timeout, DNS, …).
    Unreachable(String),
}

/// Resolved probe settings.
#[derive(Debug)]
struct Probe {
    url: String,
    expected_status: u16,
    attempts: u32,
    retry_delay: Duration,
    timeout: Duration,
}

impl Probe {
    /// Resolve the probe configuration for `host` from `ctx.settings`.
    // Reason: `{host}` / `{host.address}` are intentional template tokens, not
    // format arguments.
    #[allow(clippy::literal_string_with_formatting_args)]
    fn from_ctx(ctx: &AdapterCtx, host: &HostId) -> Result<Self, AdapterError> {
        // The multi-host composition sets `address` per host; fall back to the host
        // id so a `{host.address}` template still resolves single-host.
        let address = ctx
            .settings
            .get("address")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| host.as_str());
        let url = ctx
            .settings
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                error(
                    AdapterErrorKind::InvalidConfig,
                    ADAPTER_NAME,
                    "check",
                    "no 'url' configured in [health] settings".to_owned(),
                    None,
                )
            })?
            .replace("{host.address}", address)
            .replace("{host}", host.as_str());

        let expected_status =
            u16_setting(ctx, "expected_status").unwrap_or(DEFAULT_EXPECTED_STATUS);
        let attempts = u64_setting(ctx, "attempts")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(DEFAULT_ATTEMPTS)
            .max(1);
        let retry_delay = Duration::from_millis(
            u64_setting(ctx, "retry_delay_ms").unwrap_or(DEFAULT_RETRY_DELAY_MS),
        );
        let timeout =
            Duration::from_millis(u64_setting(ctx, "timeout_ms").unwrap_or(DEFAULT_TIMEOUT_MS));

        Ok(Self {
            url,
            expected_status,
            attempts,
            retry_delay,
            timeout,
        })
    }
}

/// Read an unsigned integer setting, if present and in range.
fn u64_setting(ctx: &AdapterCtx, key: &str) -> Option<u64> {
    ctx.settings.get(key).and_then(Value::as_u64)
}

/// Read a `u16` setting, if present and in range.
fn u16_setting(ctx: &AdapterCtx, key: &str) -> Option<u16> {
    u64_setting(ctx, key).and_then(|value| u16::try_from(value).ok())
}

/// The HTTP health-probe adapter.
///
/// # Example
/// ```
/// use fraisier_adapter_http::HttpHealth;
///
/// let adapter = HttpHealth::new();
/// let _ = adapter;
/// ```
#[derive(Default)]
pub struct HttpHealth {
    /// The resolved `(header, value)` from the configured token provider, cached
    /// so the provider is resolved **at most once per deploy** even though `check`
    /// runs per host. Empty when no token provider is configured.
    token: OnceCell<(String, String)>,
}

impl HttpHealth {
    /// Create an HTTP health adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            token: OnceCell::const_new(),
        }
    }

    /// Resolve the request headers for a probe: the static `[health].headers`
    /// plus, if configured, the token provider's injected header (resolved once
    /// per deploy via [`Self::token`]).
    async fn resolve_headers(
        &self,
        ctx: &AdapterCtx,
    ) -> Result<Vec<(String, String)>, AdapterError> {
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(map) = ctx.settings.get("headers").and_then(Value::as_object) {
            for (key, value) in map {
                if let Some(value) = value.as_str() {
                    headers.push((key.clone(), value.to_owned()));
                }
            }
        }
        if let Some(raw) = ctx.settings.get("token_provider") {
            let provider: TokenProvider = serde_json::from_value(raw.clone()).map_err(|err| {
                error(
                    AdapterErrorKind::InvalidConfig,
                    ADAPTER_NAME,
                    "token",
                    format!("invalid token_provider config: {err}"),
                    None,
                )
            })?;
            let (name, value) = self
                .token
                .get_or_try_init(|| token::resolve_header(&provider, ctx))
                .await?
                .clone();
            headers.push((name, value));
        }
        Ok(headers)
    }
}

/// Perform one probe: `Ok(status)` when healthy, else the failure reason.
async fn probe_once(
    client: &reqwest::Client,
    url: &str,
    expected: u16,
    headers: &[(String, String)],
) -> Result<u16, ProbeFailure> {
    let mut request = client.get(url);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            if status == expected {
                Ok(status)
            } else {
                Err(ProbeFailure::Unhealthy(status))
            }
        }
        Err(err) => Err(ProbeFailure::Unreachable(err.to_string())),
    }
}

#[async_trait]
impl HealthAdapter for HttpHealth {
    async fn check(&self, ctx: &AdapterCtx, host: &HostId) -> Result<HealthStatus, AdapterError> {
        let probe = Probe::from_ctx(ctx, host)?;
        // Static headers + a (once-per-deploy) token-provider header, if configured.
        let headers = self.resolve_headers(ctx).await?;
        let client = reqwest::Client::builder()
            .timeout(probe.timeout)
            .build()
            .map_err(|err| {
                error(
                    AdapterErrorKind::Execution,
                    ADAPTER_NAME,
                    "check",
                    format!("failed to build HTTP client: {err}"),
                    None,
                )
            })?;

        let result = retry_on_err(probe.attempts, probe.retry_delay, || {
            probe_once(&client, &probe.url, probe.expected_status, &headers)
        })
        .await;

        match result {
            Ok(status) => Ok(HealthStatus {
                healthy: true,
                detail: Some(format!("HTTP {status}")),
            }),
            Err(ProbeFailure::Unhealthy(status)) => Ok(HealthStatus {
                healthy: false,
                detail: Some(format!(
                    "HTTP {status} (expected {})",
                    probe.expected_status
                )),
            }),
            Err(ProbeFailure::Unreachable(message)) => Err(error(
                AdapterErrorKind::Execution,
                ADAPTER_NAME,
                "check",
                format!("health probe of '{}' failed: {message}", probe.url),
                None,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Probe;
    use fraisier_core::adapter_axes::{AdapterCtx, HostId};
    use serde_json::json;

    #[test]
    fn probe_requires_a_url() {
        let ctx = AdapterCtx::new("checkout", "production");
        let err = Probe::from_ctx(&ctx, &HostId::new("web-1")).expect_err("missing url");
        assert_eq!(err.adapter.as_deref(), Some("http"));
    }

    #[test]
    fn probe_substitutes_host_and_applies_defaults() {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.settings.insert(
            "url".to_owned(),
            json!("http://{host}.internal:8080/health"),
        );
        let probe = Probe::from_ctx(&ctx, &HostId::new("web-2")).expect("probe");
        assert_eq!(probe.url, "http://web-2.internal:8080/health");
        assert_eq!(probe.expected_status, 200);
        assert_eq!(probe.attempts, 3);
    }

    #[test]
    fn probe_substitutes_the_per_host_address() {
        // The multi-host composition sets `address` per host; `{host.address}`
        // resolves to it so each host is probed at its own address.
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.settings
            .insert("url".to_owned(), json!("http://{host.address}:8080/health"));
        ctx.settings
            .insert("address".to_owned(), json!("127.0.0.2"));
        let probe = Probe::from_ctx(&ctx, &HostId::new("web-1")).expect("probe");
        assert_eq!(probe.url, "http://127.0.0.2:8080/health");
    }

    #[test]
    fn host_address_falls_back_to_the_host_id_when_unset() {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.settings
            .insert("url".to_owned(), json!("http://{host.address}/health"));
        let probe = Probe::from_ctx(&ctx, &HostId::new("solo.internal")).expect("probe");
        assert_eq!(probe.url, "http://solo.internal/health");
    }

    #[test]
    fn probe_reads_overrides() {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.settings
            .insert("url".to_owned(), json!("http://x/health"));
        ctx.settings
            .insert("expected_status".to_owned(), json!(204));
        ctx.settings.insert("attempts".to_owned(), json!(1));
        ctx.settings.insert("timeout_ms".to_owned(), json!(250));
        let probe = Probe::from_ctx(&ctx, &HostId::new("web-1")).expect("probe");
        assert_eq!(probe.expected_status, 204);
        assert_eq!(probe.attempts, 1);
        assert_eq!(probe.timeout.as_millis(), 250);
    }
}

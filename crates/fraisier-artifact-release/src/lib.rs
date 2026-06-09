//! # fraisier-artifact-release
//!
//! The [`ReleaseArtifact`] adapter: an [`ArtifactAdapter`] that fetches a release
//! archive over HTTP, verifies its SHA-256, stages it, and activates it via an
//! atomic symlink swap (PRD §6.3, the `release` artifact source).
//!
//! ## Configuration
//!
//! Read per call from [`AdapterCtx::settings`] (the `[artifact]` table):
//!
//! ```toml
//! [artifact]
//! source = "release"
//! version = "1.2.3"
//! release_url = "https://example.com/app-{version}.tar.gz"   # {version} substituted
//! sha256 = "<hex>"                # inline checksum, OR:
//! checksum_url = "https://example.com/app-{version}.tar.gz.sha256"
//! staging_dir = "/var/lib/app/staging"   # default <workdir>/.fraisier-staging
//! active_path = "/var/lib/app/current"   # the symlink swapped on activate
//! ```
//!
//! ## Safety
//!
//! When a checksum is configured (inline `sha256` or fetched from
//! `checksum_url`), a mismatch aborts staging — a corrupted or tampered download
//! is never staged. Activation replaces the `active_path` symlink atomically
//! (write a temp link, then `rename` over the target), so a deploy never sees a
//! half-updated `current`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use fraisier_adapter_support::{error, retry_on_err, staging};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterError, AdapterErrorKind, ArtifactAdapter, ArtifactRef, HostId,
    StagedArtifact,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

/// The adapter's identity name.
const ADAPTER_NAME: &str = "release";

const DEFAULT_ATTEMPTS: u32 = 3;
const DEFAULT_RETRY_DELAY_MS: u64 = 500;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Resolved release configuration.
#[derive(Debug)]
struct Config {
    version: String,
    release_url: String,
    sha256: Option<String>,
    checksum_url: Option<String>,
    staging_dir: PathBuf,
    active_path: Option<PathBuf>,
    attempts: u32,
    retry_delay: Duration,
    timeout: Duration,
}

impl Config {
    /// Resolve the configuration from `ctx.settings`, substituting `{version}`.
    // Reason: `{version}` is an intentional template token, not a format argument.
    #[allow(clippy::literal_string_with_formatting_args)]
    fn from_ctx(ctx: &AdapterCtx, operation: &str) -> Result<Self, AdapterError> {
        let version = string_setting(ctx, "version")
            .ok_or_else(|| invalid(operation, "no 'version' configured in [artifact] settings"))?;
        let release_url = string_setting(ctx, "release_url")
            .ok_or_else(|| {
                invalid(
                    operation,
                    "no 'release_url' configured in [artifact] settings",
                )
            })?
            .replace("{version}", &version);
        let checksum_url =
            string_setting(ctx, "checksum_url").map(|url| url.replace("{version}", &version));
        let staging_dir = string_setting(ctx, "staging_dir")
            .map_or_else(|| ctx.workdir.join(".fraisier-staging"), PathBuf::from);
        let attempts = u64_setting(ctx, "attempts")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(DEFAULT_ATTEMPTS)
            .max(1);

        Ok(Self {
            version,
            release_url,
            sha256: string_setting(ctx, "sha256"),
            checksum_url,
            staging_dir,
            active_path: string_setting(ctx, "active_path").map(PathBuf::from),
            attempts,
            retry_delay: Duration::from_millis(
                u64_setting(ctx, "retry_delay_ms").unwrap_or(DEFAULT_RETRY_DELAY_MS),
            ),
            timeout: Duration::from_millis(
                u64_setting(ctx, "timeout_ms").unwrap_or(DEFAULT_TIMEOUT_MS),
            ),
        })
    }

    /// The path the artifact is staged at.
    fn staged_path(&self) -> PathBuf {
        self.staging_dir.join(&self.version)
    }

    /// The configured `active_path`, or an error if absent (`activate`/`current`).
    fn require_active_path(&self, operation: &str) -> Result<&Path, AdapterError> {
        self.active_path.as_deref().ok_or_else(|| {
            invalid(
                operation,
                "no 'active_path' configured in [artifact] settings",
            )
        })
    }
}

/// Read a non-empty string setting.
fn string_setting(ctx: &AdapterCtx, key: &str) -> Option<String> {
    ctx.settings
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Read an unsigned integer setting.
fn u64_setting(ctx: &AdapterCtx, key: &str) -> Option<u64> {
    ctx.settings.get(key).and_then(Value::as_u64)
}

/// Build an `InvalidConfig` error tagged with the adapter and operation.
fn invalid(operation: &str, message: &str) -> AdapterError {
    error(
        AdapterErrorKind::InvalidConfig,
        ADAPTER_NAME,
        operation,
        message.to_owned(),
        None,
    )
}

/// Build an `Execution` error tagged with the adapter and operation.
fn execution(operation: &str, message: String) -> AdapterError {
    error(
        AdapterErrorKind::Execution,
        ADAPTER_NAME,
        operation,
        message,
        None,
    )
}

/// Lower-case hex encoding of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut acc, byte| {
        let _ = write!(acc, "{byte:02x}");
        acc
    })
}

/// The release-artifact adapter.
///
/// # Example
/// ```
/// use fraisier_artifact_release::ReleaseArtifact;
///
/// let adapter = ReleaseArtifact::new();
/// let _ = adapter;
/// ```
#[derive(Default)]
pub struct ReleaseArtifact {
    _private: (),
}

impl ReleaseArtifact {
    /// Create a release-artifact adapter.
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

/// Download `url`, retrying on transport failure.
async fn download(
    client: &reqwest::Client,
    url: &str,
    attempts: u32,
    delay: Duration,
    operation: &str,
) -> Result<Vec<u8>, AdapterError> {
    retry_on_err(attempts, delay, || async {
        let response = client.get(url).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {} fetching {url}", response.status()));
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|message| execution(operation, message))
}

#[async_trait]
impl ArtifactAdapter for ReleaseArtifact {
    async fn stage(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
    ) -> Result<StagedArtifact, AdapterError> {
        let cfg = Config::from_ctx(ctx, "stage")?;
        let client = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| execution("stage", format!("failed to build HTTP client: {e}")))?;

        let bytes = download(
            &client,
            &cfg.release_url,
            cfg.attempts,
            cfg.retry_delay,
            "stage",
        )
        .await?;
        let digest = hex(&Sha256::digest(&bytes));

        // Resolve the expected checksum (inline wins over a fetched one).
        let expected = match &cfg.sha256 {
            Some(sum) => Some(sum.trim().to_ascii_lowercase()),
            None => match &cfg.checksum_url {
                Some(url) => {
                    let raw =
                        download(&client, url, cfg.attempts, cfg.retry_delay, "stage").await?;
                    let text = String::from_utf8_lossy(&raw);
                    text.split_whitespace().next().map(str::to_ascii_lowercase)
                }
                None => None,
            },
        };
        if let Some(expected) = expected {
            if expected != digest {
                return Err(execution(
                    "stage",
                    format!(
                        "checksum mismatch for {}: expected {expected}, got {digest}",
                        cfg.version
                    ),
                ));
            }
        }

        let staged_path = cfg.staged_path();
        std::fs::create_dir_all(&cfg.staging_dir)
            .map_err(|e| execution("stage", format!("failed to create staging dir: {e}")))?;
        std::fs::write(&staged_path, &bytes)
            .map_err(|e| execution("stage", format!("failed to write staged artifact: {e}")))?;

        Ok(StagedArtifact {
            artifact: ArtifactRef {
                id: cfg.version,
                checksum: Some(digest),
            },
            path: staged_path,
        })
    }

    async fn activate(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
        staged: &StagedArtifact,
    ) -> Result<(), AdapterError> {
        let cfg = Config::from_ctx(ctx, "activate")?;
        let active = cfg.require_active_path("activate")?;
        staging::activate_symlink(active, &staged.path, ADAPTER_NAME, "activate")
    }

    async fn current(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
    ) -> Result<Option<ArtifactRef>, AdapterError> {
        let cfg = Config::from_ctx(ctx, "current")?;
        let active = cfg.require_active_path("current")?;
        staging::read_active_link(active, ADAPTER_NAME, "current")
    }
}

#[cfg(test)]
mod tests {
    use super::{hex, Config};
    use fraisier_core::adapter_axes::AdapterCtx;
    use serde_json::json;

    #[test]
    fn hex_encodes_lowercase() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa9, 0xff]), "000fa9ff");
    }

    #[test]
    fn config_requires_version_and_url() {
        let ctx = AdapterCtx::new("checkout", "production");
        assert!(Config::from_ctx(&ctx, "stage").is_err());
    }

    #[test]
    fn config_substitutes_version_in_urls() {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.settings.insert("version".to_owned(), json!("1.2.3"));
        ctx.settings.insert(
            "release_url".to_owned(),
            json!("https://x/app-{version}.tar.gz"),
        );
        ctx.settings.insert(
            "checksum_url".to_owned(),
            json!("https://x/app-{version}.tar.gz.sha256"),
        );
        let cfg = Config::from_ctx(&ctx, "stage").expect("config");
        assert_eq!(cfg.release_url, "https://x/app-1.2.3.tar.gz");
        assert_eq!(
            cfg.checksum_url.as_deref(),
            Some("https://x/app-1.2.3.tar.gz.sha256")
        );
        assert!(cfg.staged_path().ends_with("1.2.3"));
    }

    #[test]
    fn active_path_required_for_activate_and_current() {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.settings.insert("version".to_owned(), json!("1.0.0"));
        ctx.settings
            .insert("release_url".to_owned(), json!("https://x/a.tgz"));
        let cfg = Config::from_ctx(&ctx, "current").expect("config");
        assert!(cfg.require_active_path("current").is_err());
    }
}

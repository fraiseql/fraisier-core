//! # fraisier-artifact-pull
//!
//! The [`PullArtifact`] adapter: an [`ArtifactAdapter`] where **each host fetches
//! and activates its own release**, by shelling out to `curl` / `sha256sum` /
//! `ln` / `mv` / `readlink` through a [`Transport`] (PRD §6.3, the host-pull
//! artifact source). With a [`Transport::Ssh`] those commands run on the remote
//! host; with the default [`Transport::Local`] they run locally. It is the
//! "simple Linux fleet" strategy: nothing to install on the hosts beyond standard
//! coreutils + `curl`, and the orchestrator never holds the bytes.
//!
//! ## Configuration (read per call from [`AdapterCtx::settings`], the `[artifact]` table)
//!
//! ```toml
//! [artifact]
//! source = "pull"
//! version = "1.2.3"
//! release_url = "https://example.com/app-{version}.tar.gz"   # {version} substituted
//! sha256 = "<hex>"               # inline checksum, OR:
//! checksum_url = "https://example.com/app-{version}.tar.gz.sha256"
//! staging_dir = "/var/lib/app/releases"   # on the host; default ".fraisier-staging"
//! active_path = "/var/lib/app/current"    # the symlink swapped on activate
//! ```
//!
//! ## Safety
//!
//! When a checksum is configured (inline `sha256` or fetched from `checksum_url`)
//! the staged file's `sha256sum` is verified on the host and a mismatch aborts
//! staging — a corrupted or tampered download is never activated. Activation is
//! atomic: a temporary symlink is created next to `active_path`, then `mv -T`
//! renames it over the target, so `current` never observes a half-updated link
//! (`staging_dir` and `active_path` must be on the same filesystem). `mv -T` /
//! `ln -sfn` are GNU coreutils — the host-pull strategy targets Linux fleets.

use std::ffi::{OsStr, OsString};

use async_trait::async_trait;
use fraisier_adapter_support::{error, Captured, Transport};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterError, AdapterErrorKind, ArtifactAdapter, ArtifactRef, HostId,
    StagedArtifact,
};
use serde_json::Value;

/// The adapter's identity name.
const ADAPTER_NAME: &str = "pull";

/// The default directory (relative to the host's login dir) releases are staged in.
const DEFAULT_STAGING_DIR: &str = ".fraisier-staging";

/// The host-pull artifact adapter.
///
/// # Example
/// ```
/// use fraisier_artifact_pull::PullArtifact;
///
/// let local = PullArtifact::new();
/// // The multi-host path builds it with a `Transport::Ssh` so each host pulls
/// // its own release:
/// // let remote = PullArtifact::new().with_transport(Transport::ssh(ssh));
/// let _ = local;
/// ```
pub struct PullArtifact {
    transport: Transport,
    curl: OsString,
}

impl Default for PullArtifact {
    fn default() -> Self {
        Self::new()
    }
}

impl PullArtifact {
    /// Create an adapter that runs `curl` (or `$FRAISIER_CURL_BIN` when set) on the
    /// **local** host.
    #[must_use]
    pub fn new() -> Self {
        let curl = std::env::var_os("FRAISIER_CURL_BIN")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("curl"));
        Self {
            transport: Transport::Local,
            curl,
        }
    }

    /// Run the host-pull commands over `transport` instead of locally (the
    /// multi-host path passes a [`Transport::Ssh`] so each host pulls its own
    /// release).
    #[must_use]
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// Override which `curl` binary is invoked (tests point this at a fake).
    #[must_use]
    pub fn with_curl(mut self, curl: impl Into<OsString>) -> Self {
        self.curl = curl.into();
        self
    }

    /// Run `program args` over the transport, succeeding only on exit 0.
    async fn exec(
        &self,
        ctx: &AdapterCtx,
        program: &OsStr,
        args: &[&str],
        operation: &str,
    ) -> Result<Captured, AdapterError> {
        let os_args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let captured = self
            .transport
            .run(ctx, program, &os_args, &[], None, ADAPTER_NAME, operation)
            .await?;
        if captured.succeeded() {
            return Ok(captured);
        }
        let code = captured
            .code
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        Err(error(
            AdapterErrorKind::Execution,
            ADAPTER_NAME,
            operation,
            format!("`{} …` exited with {code}", program.to_string_lossy()),
            captured.stderr_opt(),
        ))
    }

    /// Resolve the expected checksum: an inline `sha256` wins, else fetch
    /// `checksum_url` with `curl` on the host and take its first whitespace token.
    async fn expected_checksum(
        &self,
        ctx: &AdapterCtx,
        url_substituted: impl Fn(&str) -> String,
    ) -> Result<Option<String>, AdapterError> {
        if let Some(sum) = string_setting(ctx, "sha256") {
            return Ok(Some(sum.trim().to_ascii_lowercase()));
        }
        let Some(checksum_url) = string_setting(ctx, "checksum_url").map(|u| url_substituted(&u))
        else {
            return Ok(None);
        };
        let captured = self
            .exec(
                ctx,
                &self.curl,
                &["-fSL", "--retry", "3", &checksum_url],
                "stage",
            )
            .await?;
        Ok(captured
            .stdout
            .split_whitespace()
            .next()
            .map(str::to_ascii_lowercase))
    }
}

/// Read a non-empty string setting from `ctx.settings`.
fn string_setting(ctx: &AdapterCtx, key: &str) -> Option<String> {
    ctx.settings
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Read a required non-empty string setting, or an `InvalidConfig` error.
fn require_setting(ctx: &AdapterCtx, key: &str, operation: &str) -> Result<String, AdapterError> {
    string_setting(ctx, key).ok_or_else(|| {
        error(
            AdapterErrorKind::InvalidConfig,
            ADAPTER_NAME,
            operation,
            format!("no '{key}' configured in [artifact] settings"),
            None,
        )
    })
}

#[async_trait]
impl ArtifactAdapter for PullArtifact {
    async fn stage(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
    ) -> Result<StagedArtifact, AdapterError> {
        let version = require_setting(ctx, "version", "stage")?;
        // Reason: `{version}` is an intentional template token, not a format arg.
        #[allow(clippy::literal_string_with_formatting_args)]
        let substitute = |url: &str| url.replace("{version}", &version);
        let url = substitute(&require_setting(ctx, "release_url", "stage")?);
        let staging_dir =
            string_setting(ctx, "staging_dir").unwrap_or_else(|| DEFAULT_STAGING_DIR.to_owned());
        let staged_path = format!("{staging_dir}/{version}");

        self.exec(ctx, "mkdir".as_ref(), &["-p", &staging_dir], "stage")
            .await?;
        self.exec(
            ctx,
            &self.curl,
            &["-fSL", "--retry", "3", &url, "-o", &staged_path],
            "stage",
        )
        .await?;

        let expected = self.expected_checksum(ctx, substitute).await?;
        if let Some(expected) = &expected {
            let captured = self
                .exec(ctx, "sha256sum".as_ref(), &[&staged_path], "stage")
                .await?;
            let actual = captured
                .stdout
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if &actual != expected {
                return Err(error(
                    AdapterErrorKind::Execution,
                    ADAPTER_NAME,
                    "stage",
                    format!("checksum mismatch for {version}: expected {expected}, got {actual}"),
                    None,
                ));
            }
        }

        Ok(StagedArtifact {
            artifact: ArtifactRef {
                id: version,
                checksum: expected,
            },
            path: staged_path.into(),
        })
    }

    async fn activate(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
        staged: &StagedArtifact,
    ) -> Result<(), AdapterError> {
        let active = require_setting(ctx, "active_path", "activate")?;
        let staged_path = staged.path.to_string_lossy().into_owned();
        let tmp = format!("{active}.fraisier-tmp");

        // Atomic swap: point a temp symlink at the staged release, then rename it
        // over `active` (a same-filesystem rename is atomic), so `current` never
        // observes a half-updated link.
        self.exec(
            ctx,
            "ln".as_ref(),
            &["-sfn", &staged_path, &tmp],
            "activate",
        )
        .await?;
        self.exec(ctx, "mv".as_ref(), &["-Tf", &tmp, &active], "activate")
            .await?;
        Ok(())
    }

    async fn current(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
    ) -> Result<Option<ArtifactRef>, AdapterError> {
        let active = require_setting(ctx, "active_path", "current")?;
        let argv = [OsString::from(&active)];
        let captured = self
            .transport
            .run(
                ctx,
                "readlink".as_ref(),
                &argv,
                &[],
                None,
                ADAPTER_NAME,
                "current",
            )
            .await?;
        // `readlink` exits non-zero when `active` is absent or not a symlink — that
        // is "nothing active yet", not an error.
        if !captured.succeeded() {
            return Ok(None);
        }
        let target = captured.stdout.trim();
        Ok(target
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .map(|id| ArtifactRef {
                id: id.to_owned(),
                checksum: None,
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::PullArtifact;
    use fraisier_core::adapter_axes::{AdapterCtx, ArtifactAdapter, HostId, StagedArtifact};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    /// The bytes the fake `curl` "downloads".
    const ARTIFACT_BYTES: &[u8] = b"fraisier-test-artifact-v1";

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest as _, Sha256};
        use std::fmt::Write as _;
        Sha256::digest(bytes)
            .iter()
            .fold(String::new(), |mut acc, byte| {
                let _ = write!(acc, "{byte:02x}");
                acc
            })
    }

    /// A fake `curl` that writes [`ARTIFACT_BYTES`] to the path after its `-o` flag,
    /// ignoring the URL — so `stage` runs offline. Returns (dir kept alive, path).
    fn fake_curl() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake-curl");
        std::fs::write(
            &path,
            "#!/bin/sh\nout=\"\"; prev=\"\"\nfor a in \"$@\"; do [ \"$prev\" = \"-o\" ] && out=\"$a\"; prev=\"$a\"; done\nprintf 'fraisier-test-artifact-v1' > \"$out\"\n",
        )
        .expect("write fake curl");
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
        (dir, path)
    }

    fn ctx(root: &Path, sha256: Option<&str>) -> AdapterCtx {
        let mut ctx = AdapterCtx::new("app", "prod");
        ctx.settings.insert("version".to_owned(), json!("1.2.3"));
        ctx.settings.insert(
            "release_url".to_owned(),
            json!("https://example.com/app-{version}.tar.gz"),
        );
        ctx.settings.insert(
            "staging_dir".to_owned(),
            json!(root.join("releases").to_string_lossy()),
        );
        ctx.settings.insert(
            "active_path".to_owned(),
            json!(root.join("current").to_string_lossy()),
        );
        if let Some(sum) = sha256 {
            ctx.settings.insert("sha256".to_owned(), json!(sum));
        }
        ctx
    }

    fn adapter(curl: &Path) -> PullArtifact {
        // Default Transport::Local: the commands run locally with real coreutils.
        PullArtifact::new().with_curl(curl)
    }

    #[tokio::test]
    async fn stage_downloads_verifies_then_activate_and_current_roundtrip() {
        let (_curl_dir, curl) = fake_curl();
        let root = tempfile::tempdir().expect("root");
        let adapter = adapter(&curl);
        let host = HostId::new("web-1");

        let ctx = ctx(root.path(), Some(&sha256_hex(ARTIFACT_BYTES)));
        let staged = adapter.stage(&ctx, &host).await.expect("stage");
        assert_eq!(staged.artifact.id, "1.2.3");
        assert!(staged.path.ends_with("1.2.3"));
        assert_eq!(
            std::fs::read(&staged.path).expect("staged file"),
            ARTIFACT_BYTES
        );

        adapter
            .activate(&ctx, &host, &staged)
            .await
            .expect("activate");
        let active = root.path().join("current");
        assert_eq!(
            std::fs::read_link(&active).expect("current symlink"),
            staged.path
        );

        let current = adapter.current(&ctx, &host).await.expect("current");
        assert_eq!(current.expect("some").id, "1.2.3");
    }

    #[tokio::test]
    async fn stage_rejects_a_checksum_mismatch() {
        let (_curl_dir, curl) = fake_curl();
        let root = tempfile::tempdir().expect("root");
        let adapter = adapter(&curl);

        let ctx = ctx(
            root.path(),
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
        );
        let err = adapter
            .stage(&ctx, &HostId::new("web-1"))
            .await
            .expect_err("a bad checksum must abort staging");
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
    }

    #[tokio::test]
    async fn stage_without_a_checksum_skips_verification() {
        let (_curl_dir, curl) = fake_curl();
        let root = tempfile::tempdir().expect("root");
        let adapter = adapter(&curl);

        let staged = adapter
            .stage(&ctx(root.path(), None), &HostId::new("web-1"))
            .await
            .expect("stage with no checksum");
        assert!(staged.artifact.checksum.is_none());
    }

    #[tokio::test]
    async fn current_is_none_when_nothing_is_active() {
        let (_curl_dir, curl) = fake_curl();
        let root = tempfile::tempdir().expect("root");
        let adapter = adapter(&curl);
        let current = adapter
            .current(&ctx(root.path(), None), &HostId::new("web-1"))
            .await
            .expect("current");
        assert!(current.is_none(), "no symlink yet");
    }

    #[tokio::test]
    async fn activate_requires_active_path() {
        let (_curl_dir, curl) = fake_curl();
        let adapter = adapter(&curl);
        let mut ctx = AdapterCtx::new("app", "prod");
        ctx.settings.insert("version".to_owned(), json!("1.2.3"));
        let staged = StagedArtifact {
            artifact: fraisier_core::adapter_axes::ArtifactRef {
                id: "1.2.3".to_owned(),
                checksum: None,
            },
            path: "/tmp/x".into(),
        };
        let err = adapter
            .activate(&ctx, &HostId::new("web-1"), &staged)
            .await
            .expect_err("missing active_path");
        assert_eq!(
            err.kind,
            fraisier_core::adapter_axes::AdapterErrorKind::InvalidConfig
        );
    }
}

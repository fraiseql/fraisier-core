//! Resolving and verifying the bytes of a new fraisier binary.
//!
//! A [`Source`] is either an HTTP(S) URL or a local path. Either way the bytes
//! are SHA-256'd and, when a checksum is configured (inline or fetched from a
//! `checksum_url`), **verified-or-aborted** before they are ever staged — a
//! corrupted or tampered binary is never swapped in. With no checksum configured
//! the bytes are returned with [`Verified::verified`] set to `false` so the
//! caller can warn.

use std::path::PathBuf;
use std::time::Duration;

use fraisier_adapter_support::retry_on_err;
use sha2::{Digest as _, Sha256};

use crate::Error;

const DEFAULT_ATTEMPTS: u32 = 3;
const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_TIMEOUT: Duration = Duration::from_mins(1);

/// Where the new binary's bytes come from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Fetched over HTTP(S). The checksum is inline (`sha256`) or fetched from
    /// `checksum_url` (first whitespace-delimited token of the response).
    Url {
        /// The URL to download the binary from.
        url: String,
        /// An inline expected SHA-256 (hex), if known.
        sha256: Option<String>,
        /// A URL whose body's first token is the expected SHA-256, if used.
        checksum_url: Option<String>,
    },
    /// Already on the local filesystem (the operator staged it). The checksum is
    /// the inline `sha256`, if provided.
    Path {
        /// The path to the binary on disk.
        path: PathBuf,
        /// An inline expected SHA-256 (hex), if known.
        sha256: Option<String>,
    },
}

/// Verified bytes plus their digest and whether a checksum actually backed them.
#[derive(Debug, Clone)]
pub struct Verified {
    /// The binary's bytes.
    pub bytes: Vec<u8>,
    /// The SHA-256 of [`Self::bytes`] (hex, lower-case).
    pub digest: String,
    /// `true` iff a configured checksum was present and matched; `false` means no
    /// checksum was configured (the bytes are unverified — the caller should warn).
    pub verified: bool,
}

impl Source {
    /// Fetch the bytes and verify them against the configured checksum.
    ///
    /// # Errors
    /// [`Error::Fetch`] / [`Error::Io`] if the bytes cannot be obtained, or
    /// [`Error::ChecksumMismatch`] if a configured checksum does not match — in
    /// every error case **nothing is staged**.
    pub async fn fetch(&self) -> Result<Verified, Error> {
        let (bytes, expected) = match self {
            Self::Path { path, sha256 } => {
                let bytes = tokio::fs::read(path)
                    .await
                    .map_err(|cause| Error::Io(format!("reading {}: {cause}", path.display())))?;
                (bytes, sha256.clone())
            }
            Self::Url {
                url,
                sha256,
                checksum_url,
            } => {
                let client = reqwest::Client::builder()
                    .timeout(DEFAULT_TIMEOUT)
                    .build()
                    .map_err(|cause| Error::Fetch(format!("building HTTP client: {cause}")))?;
                let bytes = download(&client, url).await?;
                let expected = match sha256 {
                    Some(sum) => Some(sum.clone()),
                    None => match checksum_url {
                        Some(checksum_url) => {
                            let raw = download(&client, checksum_url).await?;
                            String::from_utf8_lossy(&raw)
                                .split_whitespace()
                                .next()
                                .map(str::to_owned)
                        }
                        None => None,
                    },
                };
                (bytes, expected)
            }
        };

        let digest = hex(&Sha256::digest(&bytes));
        match expected {
            Some(expected) => {
                let expected = expected.trim().to_ascii_lowercase();
                if expected != digest {
                    return Err(Error::ChecksumMismatch {
                        expected,
                        actual: digest,
                    });
                }
                Ok(Verified {
                    bytes,
                    digest,
                    verified: true,
                })
            }
            None => Ok(Verified {
                bytes,
                digest,
                verified: false,
            }),
        }
    }
}

/// Download `url`, retrying on transport failure.
async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, Error> {
    retry_on_err(DEFAULT_ATTEMPTS, DEFAULT_RETRY_DELAY, || async {
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
    .map_err(Error::Fetch)
}

/// Lower-case hex of a byte slice.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut acc, byte| {
        let _ = write!(acc, "{byte:02x}");
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::{hex, Source};
    use crate::Error;

    /// A well-formed (64-hex) but deliberately-wrong checksum — it is the
    /// SHA-256 of the empty input, which the test bytes never hash to.
    const WRONG_CHECKSUM: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn write_binary(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fraisier-new");
        std::fs::write(&path, bytes).expect("write binary");
        (dir, path)
    }

    fn sha256_of(bytes: &[u8]) -> String {
        use sha2::{Digest as _, Sha256};
        hex(&Sha256::digest(bytes))
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa9, 0xff]), "000fa9ff");
    }

    #[tokio::test]
    async fn a_local_path_with_a_matching_checksum_verifies() {
        let bytes = b"the new fraisier binary";
        let (_dir, path) = write_binary(bytes);
        let verified = Source::Path {
            path,
            sha256: Some(sha256_of(bytes)),
        }
        .fetch()
        .await
        .expect("verifies");
        assert!(verified.verified);
        assert_eq!(verified.bytes, bytes);
        assert_eq!(verified.digest, sha256_of(bytes));
    }

    #[tokio::test]
    async fn a_checksum_mismatch_aborts_and_stages_nothing() {
        let bytes = b"the new fraisier binary";
        let (_dir, path) = write_binary(bytes);
        let err = Source::Path {
            path,
            // A complete (well-formed) but *wrong* checksum.
            sha256: Some(WRONG_CHECKSUM.to_owned()),
        }
        .fetch()
        .await
        .expect_err("a mismatch must abort");
        assert!(
            matches!(err, Error::ChecksumMismatch { .. }),
            "expected ChecksumMismatch, got {err:?}"
        );
    }

    #[tokio::test]
    async fn an_uppercase_or_padded_checksum_still_matches() {
        let bytes = b"case-insensitive";
        let (_dir, path) = write_binary(bytes);
        let verified = Source::Path {
            path,
            sha256: Some(format!("  {}  ", sha256_of(bytes).to_ascii_uppercase())),
        }
        .fetch()
        .await
        .expect("trim + lowercase before comparing");
        assert!(verified.verified);
    }

    #[tokio::test]
    async fn no_checksum_returns_unverified_bytes() {
        let bytes = b"unverified";
        let (_dir, path) = write_binary(bytes);
        let verified = Source::Path { path, sha256: None }
            .fetch()
            .await
            .expect("bytes returned");
        assert!(!verified.verified, "no checksum -> caller must warn");
        assert_eq!(verified.bytes, bytes);
    }

    #[tokio::test]
    async fn a_missing_local_path_is_a_fetch_error_not_a_panic() {
        let err = Source::Path {
            path: "/no/such/fraisier/binary".into(),
            sha256: None,
        }
        .fetch()
        .await
        .expect_err("missing file");
        assert!(matches!(err, Error::Io(_)), "got {err:?}");
    }
}

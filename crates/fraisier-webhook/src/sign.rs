//! The signed-request scheme: HMAC-SHA256 over `"<timestamp>.<body>"` with a
//! replay window. Verification is constant-time (the password-grade comparison
//! is the whole point), and the timestamp is folded into the signature so a
//! captured request cannot be replayed outside the tolerance.
//!
//! A client sends two headers:
//! - [`TIMESTAMP_HEADER`] — the Unix-seconds timestamp it signed with;
//! - [`SIGNATURE_HEADER`] — `sha256=<hex>` of [`sign`] over that timestamp + body.
//!
//! The signature covers the exact timestamp *string* the header carries, so the
//! client and server must use the same decimal rendering (what [`sign`] emits).

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// The header carrying the Unix-seconds timestamp the request was signed with.
pub const TIMESTAMP_HEADER: &str = "x-fraisier-timestamp";

/// The header carrying the `sha256=<hex>` signature.
pub const SIGNATURE_HEADER: &str = "x-fraisier-signature";

/// HMAC-SHA256 keyed by the shared secret. HMAC accepts any key length.
type HmacSha256 = Hmac<Sha256>;

/// Why a signed request was rejected. Carries no secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The timestamp header was absent.
    MissingTimestamp,
    /// The timestamp header was not a Unix-seconds integer.
    BadTimestamp,
    /// The timestamp is outside the replay tolerance (`skew` = now − timestamp).
    Stale {
        /// Seconds between the server clock and the signed timestamp.
        skew: i64,
    },
    /// The signature header was absent.
    MissingSignature,
    /// The signature header was not `sha256=<hex>` with valid hex.
    BadSignatureFormat,
    /// The signature did not match (wrong secret or tampered body/timestamp).
    Mismatch,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTimestamp => write!(f, "missing {TIMESTAMP_HEADER} header"),
            Self::BadTimestamp => write!(f, "malformed {TIMESTAMP_HEADER} header"),
            Self::Stale { skew } => write!(f, "timestamp outside the replay window (skew {skew}s)"),
            Self::MissingSignature => write!(f, "missing {SIGNATURE_HEADER} header"),
            Self::BadSignatureFormat => {
                write!(f, "malformed {SIGNATURE_HEADER} (want sha256=<hex>)")
            }
            Self::Mismatch => write!(f, "signature mismatch"),
        }
    }
}

impl std::error::Error for Rejection {}

/// Compute the signature header value (`sha256=<hex>`) for `body` at `timestamp`.
///
/// The same routine a client (or a test) uses to sign a request.
#[must_use]
pub fn sign(secret: &[u8], timestamp: u64, body: &[u8]) -> String {
    format!(
        "sha256={}",
        hex::encode(tag(secret, &timestamp.to_string(), body))
    )
}

/// A keyed HMAC instance. `new_from_slice` only errors for fixed-size key types;
/// HMAC takes a key of any length, so this never fails (the panic is unreachable
/// — keeping it in this private helper keeps the public API panic-free).
fn new_mac(secret: &[u8]) -> HmacSha256 {
    HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length")
}

/// The raw HMAC tag over `"<timestamp>.<body>"`.
fn tag(secret: &[u8], timestamp: &str, body: &[u8]) -> Vec<u8> {
    let mut mac = new_mac(secret);
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    mac.finalize().into_bytes().to_vec()
}

/// Verify a signed webhook request.
///
/// Checks the timestamp is present, parseable, and within `tolerance_secs` of
/// `now` (in either direction, for clock skew), then verifies the signature in
/// constant time against `HMAC_SHA256(secret, "<timestamp>.<body>")`.
///
/// # Errors
/// A [`Rejection`] describing the first failed check. The signature comparison
/// itself is constant-time; the ordering of the cheaper checks is not secret.
pub fn verify(
    secret: &[u8],
    body: &[u8],
    timestamp_header: Option<&str>,
    signature_header: Option<&str>,
    now: u64,
    tolerance_secs: u64,
) -> Result<(), Rejection> {
    let timestamp = timestamp_header.ok_or(Rejection::MissingTimestamp)?.trim();
    let parsed: u64 = timestamp.parse().map_err(|_| Rejection::BadTimestamp)?;

    // i64 skew so a future timestamp (negative skew) is handled symmetrically.
    let now_i = i64::try_from(now).unwrap_or(i64::MAX);
    let ts_i = i64::try_from(parsed).unwrap_or(i64::MAX);
    let skew = now_i - ts_i;
    if skew.unsigned_abs() > tolerance_secs {
        return Err(Rejection::Stale { skew });
    }

    let signature = signature_header.ok_or(Rejection::MissingSignature)?.trim();
    let provided_hex = signature
        .strip_prefix("sha256=")
        .ok_or(Rejection::BadSignatureFormat)?;
    let provided = hex::decode(provided_hex).map_err(|_| Rejection::BadSignatureFormat)?;

    let mut mac = new_mac(secret);
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    mac.verify_slice(&provided).map_err(|_| Rejection::Mismatch)
}

#[cfg(test)]
mod tests {
    use super::{sign, verify, Rejection};

    const SECRET: &[u8] = b"s3cr3t-webhook-key";
    const BODY: &[u8] = br#"{"version":"1.2.3"}"#;
    const NOW: u64 = 1_700_000_000;
    const TOL: u64 = 300;

    fn headers(ts: u64, body: &[u8]) -> (String, String) {
        (ts.to_string(), sign(SECRET, ts, body))
    }

    #[test]
    fn a_freshly_signed_request_verifies() {
        let (ts, sig) = headers(NOW, BODY);
        verify(SECRET, BODY, Some(&ts), Some(&sig), NOW, TOL).expect("valid");
    }

    #[test]
    fn a_tampered_body_is_rejected() {
        let (ts, sig) = headers(NOW, BODY);
        let err = verify(
            SECRET,
            b"{\"version\":\"9.9.9\"}",
            Some(&ts),
            Some(&sig),
            NOW,
            TOL,
        )
        .expect_err("tampered");
        assert_eq!(err, Rejection::Mismatch);
    }

    #[test]
    fn the_wrong_secret_is_rejected() {
        let (ts, sig) = headers(NOW, BODY);
        let err =
            verify(b"other-secret", BODY, Some(&ts), Some(&sig), NOW, TOL).expect_err("wrong key");
        assert_eq!(err, Rejection::Mismatch);
    }

    #[test]
    fn a_replayed_old_request_is_rejected() {
        let old = NOW - TOL - 1;
        let (ts, sig) = headers(old, BODY);
        let err = verify(SECRET, BODY, Some(&ts), Some(&sig), NOW, TOL).expect_err("stale");
        assert!(matches!(err, Rejection::Stale { .. }), "{err:?}");
    }

    #[test]
    fn a_future_timestamp_beyond_tolerance_is_rejected() {
        let future = NOW + TOL + 1;
        let (ts, sig) = headers(future, BODY);
        let err = verify(SECRET, BODY, Some(&ts), Some(&sig), NOW, TOL).expect_err("future");
        assert!(matches!(err, Rejection::Stale { .. }), "{err:?}");
    }

    #[test]
    fn timestamps_within_tolerance_pass_in_both_directions() {
        for ts in [NOW - TOL, NOW + TOL, NOW - 1, NOW + 1] {
            let (ts_s, sig) = headers(ts, BODY);
            verify(SECRET, BODY, Some(&ts_s), Some(&sig), NOW, TOL)
                .unwrap_or_else(|e| panic!("ts {ts} within tolerance should pass: {e:?}"));
        }
    }

    #[test]
    fn missing_headers_are_rejected() {
        let (ts, sig) = headers(NOW, BODY);
        assert_eq!(
            verify(SECRET, BODY, None, Some(&sig), NOW, TOL).expect_err("no ts"),
            Rejection::MissingTimestamp
        );
        assert_eq!(
            verify(SECRET, BODY, Some(&ts), None, NOW, TOL).expect_err("no sig"),
            Rejection::MissingSignature
        );
    }

    #[test]
    fn malformed_timestamp_and_signature_are_rejected() {
        let (ts, sig) = headers(NOW, BODY);
        assert_eq!(
            verify(SECRET, BODY, Some("not-a-number"), Some(&sig), NOW, TOL).expect_err("bad ts"),
            Rejection::BadTimestamp
        );
        assert_eq!(
            verify(SECRET, BODY, Some(&ts), Some("deadbeef"), NOW, TOL).expect_err("no prefix"),
            Rejection::BadSignatureFormat
        );
        assert_eq!(
            verify(SECRET, BODY, Some(&ts), Some("sha256=zzzz"), NOW, TOL).expect_err("bad hex"),
            Rejection::BadSignatureFormat
        );
    }
}

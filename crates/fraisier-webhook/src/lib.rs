//! # fraisier-webhook
//!
//! The webhook server (PRD §3.4, G10): HMAC-signed, replay-protected HTTP POSTs
//! that trigger a deploy, served over systemd **socket activation** or a
//! **standalone** listener.
//!
//! This crate is the transport + verification mechanism only; what a verified
//! request *does* (run a deploy) is a callback the CLI supplies, so the crate
//! stays independent of the deploy composition.
//!
//! - [`sign`] / [`verify`] — the signed-request scheme (HMAC-SHA256 over
//!   `"<timestamp>.<body>"` with a replay window), the security core.

mod sign;

pub use sign::{sign, verify, Rejection, SIGNATURE_HEADER, TIMESTAMP_HEADER};

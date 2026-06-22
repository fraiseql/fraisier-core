//! Smoke-test / health-probe **token providers**: acquire a short-lived bearer
//! credential at deploy time and inject it into a probe's request header.
//!
//! This module owns only the *structural* model and validation (no network, no
//! subprocess) so it can live in the dependency-light core and be shared by
//! `fraisier-config` (the typed `[health].token_provider` field + `validate-config`)
//! and `fraisier-adapter-http` (which performs the actual resolution). Three
//! provider types match the Python fraisier surface:
//!
//! - `exec` — run a configured argv (no shell); stdout (trailing newline stripped)
//!   is the token.
//! - `oauth2_client_credentials` — POST `grant_type=client_credentials`.
//! - `oauth2_refresh_token` — POST `grant_type=refresh_token`.
//!
//! Secrets (the OAuth2 `client_secret` / `refresh_token`) are referenced by the
//! *source env var name* and resolved at use time via [`AdapterCtx::secret`]
//! (Decision 5) — never carried as values here.

// Reason: `{token}` is the literal injection placeholder this module manipulates,
// not a Rust format argument (same pattern as the http adapter's `{host}`).
#![allow(clippy::literal_string_with_formatting_args)]

use serde::{Deserialize, Serialize};

/// The default header a resolved token is injected into.
pub const DEFAULT_TOKEN_HEADER: &str = "Authorization";
/// The default injection template; `{token}` is replaced with the resolved token.
pub const DEFAULT_TOKEN_FORMAT: &str = "Bearer {token}";

/// The logical secret name the OAuth2 `client_secret` is resolved under.
pub const CLIENT_SECRET_LOGICAL: &str = "HEALTH_TOKEN_CLIENT_SECRET";
/// The logical secret name the OAuth2 `refresh_token` is resolved under.
pub const REFRESH_TOKEN_LOGICAL: &str = "HEALTH_TOKEN_REFRESH_TOKEN";

fn default_header() -> String {
    DEFAULT_TOKEN_HEADER.to_owned()
}

fn default_format() -> String {
    DEFAULT_TOKEN_FORMAT.to_owned()
}

/// An `exec` token provider: run `command` (argv, no shell); stdout is the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecProvider {
    /// The argv to run (no shell). `command[0]` is the program.
    pub command: Vec<String>,
    /// The header to inject the token into (default `Authorization`).
    #[serde(default = "default_header")]
    pub header: String,
    /// The injection template; must contain exactly `{token}` (default
    /// `Bearer {token}`).
    #[serde(default = "default_format")]
    pub format: String,
    /// Per-resolution timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// An `oauth2_client_credentials` provider: client-credentials grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientCredentialsProvider {
    /// The token endpoint to POST to.
    pub token_url: String,
    /// The OAuth2 client id.
    pub client_id: String,
    /// The *source env var name* holding the client secret (resolved via
    /// [`AdapterCtx::secret`] under [`CLIENT_SECRET_LOGICAL`]).
    pub client_secret_env: String,
    /// Optional `audience` form field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Optional `scope` form field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The header to inject the token into (default `Authorization`).
    #[serde(default = "default_header")]
    pub header: String,
    /// The injection template; must contain exactly `{token}`.
    #[serde(default = "default_format")]
    pub format: String,
    /// Per-resolution timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// An `oauth2_refresh_token` provider: refresh-token grant. Rotated refresh
/// tokens in the response are **discarded** (fraisier never writes secrets back).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshTokenProvider {
    /// The token endpoint to POST to.
    pub token_url: String,
    /// The OAuth2 client id.
    pub client_id: String,
    /// The *source env var name* holding the refresh token (resolved via
    /// [`AdapterCtx::secret`] under [`REFRESH_TOKEN_LOGICAL`]).
    pub refresh_token_env: String,
    /// The header to inject the token into (default `Authorization`).
    #[serde(default = "default_header")]
    pub header: String,
    /// The injection template; must contain exactly `{token}`.
    #[serde(default = "default_format")]
    pub format: String,
    /// Per-resolution timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// A configured token provider. Tagged on `type`; each variant carries only its
/// own fields (no sentinel `Option`s for sibling types).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TokenProvider {
    /// Run a configured argv; stdout is the token.
    Exec(ExecProvider),
    /// OAuth2 client-credentials grant.
    Oauth2ClientCredentials(ClientCredentialsProvider),
    /// OAuth2 refresh-token grant.
    Oauth2RefreshToken(RefreshTokenProvider),
}

/// A structural problem with a [`TokenProvider`] configuration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum TokenProviderError {
    /// The `format` template is not exactly one `{token}` placeholder.
    #[error(
        "token_provider.format must contain exactly the placeholder `{{token}}` and no other \
         (got {0:?}); e.g. \"Bearer {{token}}\""
    )]
    BadFormat(String),
    /// An `exec` provider had an empty `command`.
    #[error("token_provider.command must be a non-empty argv")]
    EmptyCommand,
    /// The provider injects into a header also set in the entry's static `headers`.
    #[error(
        "token_provider injects header {0:?}, which is also set in [health].headers \
         (case-insensitive collision); remove one"
    )]
    HeaderCollision(String),
}

impl TokenProvider {
    /// The header this provider injects the resolved token into.
    #[must_use]
    pub fn header(&self) -> &str {
        match self {
            Self::Exec(p) => &p.header,
            Self::Oauth2ClientCredentials(p) => &p.header,
            Self::Oauth2RefreshToken(p) => &p.header,
        }
    }

    /// The injection template (`{token}` is replaced with the resolved token).
    #[must_use]
    pub fn format(&self) -> &str {
        match self {
            Self::Exec(p) => &p.format,
            Self::Oauth2ClientCredentials(p) => &p.format,
            Self::Oauth2RefreshToken(p) => &p.format,
        }
    }

    /// Apply the `format` template to a resolved `token`.
    #[must_use]
    pub fn apply_format(&self, token: &str) -> String {
        self.format().replace("{token}", token)
    }

    /// Validate the provider in isolation: a well-formed `format` and (for `exec`)
    /// a non-empty `command`.
    ///
    /// # Errors
    /// [`TokenProviderError::BadFormat`] / [`TokenProviderError::EmptyCommand`].
    pub fn validate(&self) -> Result<(), TokenProviderError> {
        if let Self::Exec(p) = self {
            if p.command.is_empty() {
                return Err(TokenProviderError::EmptyCommand);
            }
        }
        validate_format(self.format())
    }

    /// Validate against the entry's static `headers`: the injected header must not
    /// collide (case-insensitively) with any static header key.
    ///
    /// # Errors
    /// [`TokenProviderError::HeaderCollision`] when the injected header is also a
    /// static header.
    pub fn validate_against_headers<'a>(
        &self,
        header_keys: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), TokenProviderError> {
        let injected = self.header().to_ascii_lowercase();
        for key in header_keys {
            if key.to_ascii_lowercase() == injected {
                return Err(TokenProviderError::HeaderCollision(
                    self.header().to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// A `format` template is valid iff it contains exactly one placeholder and that
/// placeholder is `{token}`.
fn validate_format(format: &str) -> Result<(), TokenProviderError> {
    let one_open = format.matches('{').count() == 1;
    let one_close = format.matches('}').count() == 1;
    if one_open && one_close && format.contains("{token}") {
        Ok(())
    } else {
        Err(TokenProviderError::BadFormat(format.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::{TokenProvider, TokenProviderError, DEFAULT_TOKEN_FORMAT, DEFAULT_TOKEN_HEADER};

    use serde_json::json;

    fn parse(value: serde_json::Value) -> Result<TokenProvider, serde_json::Error> {
        serde_json::from_value(value)
    }

    #[test]
    fn parses_each_provider_type_with_defaults() {
        let exec = parse(json!({"type": "exec", "command": ["vault", "token"]})).expect("exec");
        match &exec {
            TokenProvider::Exec(p) => {
                assert_eq!(p.command, ["vault", "token"]);
                assert_eq!(p.header, DEFAULT_TOKEN_HEADER);
                assert_eq!(p.format, DEFAULT_TOKEN_FORMAT);
            }
            other => panic!("expected exec, got {other:?}"),
        }
        assert!(exec.validate().is_ok());

        let cc = parse(json!({
            "type": "oauth2_client_credentials",
            "token_url": "https://idp/token",
            "client_id": "svc",
            "client_secret_env": "IDP_SECRET",
            "scope": "api",
        }))
        .expect("client_credentials");
        assert!(matches!(cc, TokenProvider::Oauth2ClientCredentials(_)));
        assert!(cc.validate().is_ok());

        let rt = parse(json!({
            "type": "oauth2_refresh_token",
            "token_url": "https://idp/token",
            "client_id": "svc",
            "refresh_token_env": "IDP_REFRESH",
        }))
        .expect("refresh_token");
        assert!(matches!(rt, TokenProvider::Oauth2RefreshToken(_)));
    }

    #[test]
    fn unknown_keys_are_rejected_per_type() {
        let err = parse(json!({"type": "exec", "command": ["x"], "cwd": "/tmp"}))
            .expect_err("cwd is not a valid exec key");
        assert!(
            err.to_string().contains("cwd") || err.to_string().contains("unknown"),
            "error should name the unknown key: {err}"
        );
    }

    #[test]
    fn bad_format_is_rejected() {
        for bad in ["Bearer ABC", "Bearer {access_token}", "{token} and {extra}"] {
            let exec = parse(json!({"type": "exec", "command": ["x"], "format": bad}))
                .expect("parses structurally");
            assert_eq!(
                exec.validate(),
                Err(TokenProviderError::BadFormat((*bad).to_owned())),
                "format {bad:?} must be rejected"
            );
        }
        let good = parse(json!({"type": "exec", "command": ["x"], "format": "Token {token}"}))
            .expect("parses");
        assert!(good.validate().is_ok());
    }

    #[test]
    fn empty_command_is_rejected() {
        let exec = parse(json!({"type": "exec", "command": []})).expect("parses");
        assert_eq!(exec.validate(), Err(TokenProviderError::EmptyCommand));
    }

    #[test]
    fn header_collision_is_case_insensitive() {
        let exec = parse(json!({"type": "exec", "command": ["x"]})).expect("parses");
        // Default header is `Authorization`; a static `authorization` collides.
        assert_eq!(
            exec.validate_against_headers(["authorization"]),
            Err(TokenProviderError::HeaderCollision(
                "Authorization".to_owned()
            ))
        );
        assert!(exec.validate_against_headers(["X-Trace-Id"]).is_ok());
    }

    #[test]
    fn apply_format_substitutes_the_token() {
        let exec = parse(json!({"type": "exec", "command": ["x"]})).expect("parses");
        assert_eq!(exec.apply_format("abc123"), "Bearer abc123");
    }
}

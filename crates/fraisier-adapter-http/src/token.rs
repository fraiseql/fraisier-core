//! Deploy-time resolution of a [`TokenProvider`] into a bearer credential, and
//! its injection into the health probe.
//!
//! Security invariants (mirroring Python fraisier 0.22):
//! - the resolved token is **never** logged or placed in an error message;
//! - an `exec` provider's stderr is **never** included in the error (a `set -x`
//!   wrapper could leak the token there);
//! - an OAuth2 error **never** echoes the response body (some identity providers
//!   include the `client_secret` in error envelopes);
//! - secrets (`client_secret` / `refresh_token`) are read via
//!   [`AdapterCtx::secret`] under fixed logical names — never from config values.

use std::time::Duration;

use fraisier_adapter_support::error;
use fraisier_core::adapter_axes::{AdapterCtx, AdapterError, AdapterErrorKind};
use fraisier_core::token_provider::{TokenProvider, CLIENT_SECRET_LOGICAL, REFRESH_TOKEN_LOGICAL};

const ADAPTER_NAME: &str = "http";
const DEFAULT_TOKEN_TIMEOUT: Duration = Duration::from_secs(30);

fn token_error(message: impl Into<String>) -> AdapterError {
    error(
        AdapterErrorKind::Execution,
        ADAPTER_NAME,
        "token",
        message.into(),
        None,
    )
}

/// Resolve `provider` into a `(header_name, header_value)` pair to inject, with
/// the token already substituted into the provider's `format` template.
///
/// # Errors
/// [`AdapterError`] if the provider fails to produce a token. The token and any
/// underlying secret never appear in the error.
pub async fn resolve_header(
    provider: &TokenProvider,
    ctx: &AdapterCtx,
) -> Result<(String, String), AdapterError> {
    let token = resolve_token(provider, ctx).await?;
    Ok((provider.header().to_owned(), provider.apply_format(&token)))
}

/// Resolve `provider` into the raw token string.
async fn resolve_token(provider: &TokenProvider, ctx: &AdapterCtx) -> Result<String, AdapterError> {
    match provider {
        TokenProvider::Exec(exec) => resolve_exec(&exec.command, timeout(exec.timeout_ms)).await,
        TokenProvider::Oauth2ClientCredentials(p) => {
            let secret = ctx.secret(CLIENT_SECRET_LOGICAL)?;
            let mut form = vec![
                ("grant_type", "client_credentials".to_owned()),
                ("client_id", p.client_id.clone()),
                ("client_secret", secret),
            ];
            if let Some(audience) = &p.audience {
                form.push(("audience", audience.clone()));
            }
            if let Some(scope) = &p.scope {
                form.push(("scope", scope.clone()));
            }
            post_for_token(&p.token_url, &form, timeout(p.timeout_ms)).await
        }
        TokenProvider::Oauth2RefreshToken(p) => {
            let refresh = ctx.secret(REFRESH_TOKEN_LOGICAL)?;
            let form = vec![
                ("grant_type", "refresh_token".to_owned()),
                ("client_id", p.client_id.clone()),
                ("refresh_token", refresh),
            ];
            post_for_token(&p.token_url, &form, timeout(p.timeout_ms)).await
        }
    }
}

/// The per-resolution timeout, defaulting when unset.
fn timeout(ms: Option<u64>) -> Duration {
    ms.map_or(DEFAULT_TOKEN_TIMEOUT, Duration::from_millis)
}

/// Run `command` (argv, no shell); its stdout (trailing newline stripped) is the
/// token. A non-zero exit or timeout fails — stderr is never surfaced.
async fn resolve_exec(command: &[String], timeout: Duration) -> Result<String, AdapterError> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| token_error("exec token provider has an empty command"))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    let run = cmd.output();
    let output = match tokio::time::timeout(timeout, run).await {
        Ok(Ok(output)) => output,
        Ok(Err(io)) => {
            return Err(token_error(format!(
                "exec token provider failed to start: {io}"
            )))
        }
        Err(_) => {
            return Err(token_error(format!(
                "exec token provider timed out after {}ms",
                timeout.as_millis()
            )))
        }
    };
    if !output.status.success() {
        // Deliberately omit stderr — a `set -x` wrapper could leak the token.
        return Err(token_error(format!(
            "exec token provider exited with {}",
            output
                .status
                .code()
                .map_or_else(|| "a signal".to_owned(), |c| format!("status {c}"))
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_owned())
}

/// POST `form` to `token_url` and extract `access_token` from the JSON response.
async fn post_for_token(
    token_url: &str,
    form: &[(&str, String)],
    timeout: Duration,
) -> Result<String, AdapterError> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| token_error(format!("failed to build HTTP client: {err}")))?;
    let body = form
        .iter()
        .map(|(key, value)| format!("{}={}", urlencode(key), urlencode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let response = client
        .post(token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|err| token_error(format!("token request to the IdP failed: {err}")))?;
    let status = response.status().as_u16();
    // The body may echo back the client_secret in an error envelope, so read it
    // only to parse the token — never include it in an error.
    let body = response
        .text()
        .await
        .map_err(|err| token_error(format!("reading the IdP response failed: {err}")))?;
    parse_token_response(status, &body).map_err(token_error)
}

/// `application/x-www-form-urlencoded` encoding of one component: percent-encode
/// everything except the RFC 3986 unreserved set.
fn urlencode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Extract `access_token` from a token endpoint's `(status, body)`. Pure, so the
/// redaction contract (no body in errors) is unit-testable without a network.
fn parse_token_response(status: u16, body: &str) -> Result<String, String> {
    if !(200..300).contains(&status) {
        return Err(format!("token endpoint returned HTTP {status}"));
    }
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| "token endpoint returned a non-JSON response".to_owned())?;
    value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| "token endpoint response had no access_token".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{parse_token_response, resolve_exec, resolve_header};
    use fraisier_core::adapter_axes::AdapterCtx;
    use fraisier_core::token_provider::{ExecProvider, TokenProvider};
    use std::time::Duration;

    fn exec(command: &[&str]) -> TokenProvider {
        TokenProvider::Exec(ExecProvider {
            command: command.iter().map(|s| (*s).to_owned()).collect(),
            header: "Authorization".to_owned(),
            format: "Bearer {token}".to_owned(),
            timeout_ms: Some(5_000),
        })
    }

    #[tokio::test]
    async fn exec_returns_stdout_stripped() {
        let token = resolve_exec(
            &["printf".to_owned(), "tok-123".to_owned()],
            Duration::from_secs(5),
        )
        .await
        .expect("exec token");
        assert_eq!(token, "tok-123");
    }

    #[tokio::test]
    async fn exec_nonzero_exit_errors_without_stderr() {
        // `sh -c 'echo SECRET-LEAK 1>&2; exit 7'` — the error must not contain the
        // stderr text (which a `set -x` wrapper could use to leak the token).
        let err = resolve_exec(
            &[
                "sh".to_owned(),
                "-c".to_owned(),
                "echo SECRET-LEAK 1>&2; exit 7".to_owned(),
            ],
            Duration::from_secs(5),
        )
        .await
        .expect_err("non-zero exit");
        let message = err.to_string();
        assert!(message.contains("status 7"), "names the code: {message}");
        assert!(
            !message.contains("SECRET-LEAK"),
            "stderr must not leak: {message}"
        );
    }

    #[tokio::test]
    async fn exec_injects_into_the_configured_header() {
        let ctx = AdapterCtx::new("app", "prod");
        let (header, value) = resolve_header(&exec(&["printf", "abc"]), &ctx)
            .await
            .expect("resolve");
        assert_eq!(header, "Authorization");
        assert_eq!(value, "Bearer abc");
    }

    #[test]
    fn parse_token_response_extracts_access_token() {
        assert_eq!(
            parse_token_response(200, r#"{"access_token":"xyz","token_type":"Bearer"}"#).unwrap(),
            "xyz"
        );
    }

    #[test]
    fn parse_token_response_redacts_body_on_failure() {
        // A non-2xx error envelope echoing the secret must not reach the message.
        let err = parse_token_response(401, r#"{"error":"bad","client_secret":"LEAKED"}"#)
            .expect_err("non-2xx");
        assert!(err.contains("401"), "names the status: {err}");
        assert!(!err.contains("LEAKED"), "body must not leak: {err}");

        // Missing access_token on a 2xx is an error too (no body echoed).
        let err = parse_token_response(200, r#"{"token_type":"Bearer"}"#)
            .expect_err("missing access_token");
        assert!(err.contains("no access_token"), "{err}");
    }
}

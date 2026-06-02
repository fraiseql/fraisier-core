//! # fraisier-adapter-nginx
//!
//! The reference [`LbAdapter`]: drains a host from an nginx `upstream` and
//! reattaches it, by toggling the `down` flag on the host's `server` directive
//! and reloading nginx (`nginx -s reload`).
//!
//! ## Configuration (read per call from [`AdapterCtx::settings`], the `[lb]` table)
//!
//! ```toml
//! [lb]
//! adapter = "nginx"
//! config_path = "/etc/nginx/sites-available/fraiseql"   # file holding the upstream block
//! upstream = "fraiseql_upstream"                         # the `upstream <name> { … }` to edit
//! ```
//!
//! The host is matched by address: the multi-host deploy provides it as
//! `settings["address"]`, falling back to the [`HostId`] string. A drain marks
//! `server <address>… down;`; a reattach clears `down` when the captured prior
//! membership was [`LbState::InPool`]. The edit is written atomically (a `.bak`
//! backup, then a temp file renamed over the target) so a reload never sees a
//! half-written config.
//!
//! ## Scope
//!
//! Drain/reattach toggle the `down` flag only; an existing `weight=` is preserved
//! and reported in [`LbMembership`] but not rewritten. `server` directives are
//! expected to be simple (`server <addr>[:port] [params];`, no inline comments).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use fraisier_adapter_support::{error, run_command};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterError, AdapterErrorKind, HostId, LbAdapter, LbMembership, LbState,
};
use serde_json::Value;

/// The adapter's identity/discovery name.
const ADAPTER_NAME: &str = "nginx";

/// Environment override for which `nginx` binary to invoke.
const PROGRAM_ENV: &str = "FRAISIER_NGINX_BIN";

/// The reference nginx load-balancer adapter.
///
/// # Example
/// ```
/// use fraisier_adapter_nginx::NginxLb;
///
/// let lb = NginxLb::new();
/// // Point at a specific binary (e.g. a fake in tests):
/// let pinned = NginxLb::with_program("/usr/sbin/nginx");
/// let _ = (lb, pinned);
/// ```
pub struct NginxLb {
    program: OsString,
}

impl Default for NginxLb {
    fn default() -> Self {
        Self::new()
    }
}

impl NginxLb {
    /// Create an adapter that invokes `nginx` (or `$FRAISIER_NGINX_BIN` when set).
    #[must_use]
    pub fn new() -> Self {
        let program = std::env::var_os(PROGRAM_ENV)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("nginx"));
        Self { program }
    }

    /// Create an adapter that invokes the binary at `program`.
    #[must_use]
    pub fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// Read a required string setting from the `[lb]` table.
    fn setting<'a>(
        ctx: &'a AdapterCtx,
        key: &str,
        operation: &str,
    ) -> Result<&'a str, AdapterError> {
        ctx.settings
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                error(
                    AdapterErrorKind::InvalidConfig,
                    ADAPTER_NAME,
                    operation,
                    format!("no '{key}' configured in [lb] settings"),
                    None,
                )
            })
    }

    /// The token used to match a `server` directive: the explicit `address`
    /// setting, else the host's inventory name.
    fn host_token(ctx: &AdapterCtx, host: &HostId) -> String {
        ctx.settings
            .get("address")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map_or_else(|| host.as_str().to_owned(), ToOwned::to_owned)
    }

    /// Read the config, toggle the host's `down` flag to `down`, write it back
    /// atomically, reload nginx, and return the membership the host had *before*.
    async fn apply(
        &self,
        ctx: &AdapterCtx,
        host: &HostId,
        operation: &str,
        down: bool,
    ) -> Result<LbMembership, AdapterError> {
        let config_path = PathBuf::from(Self::setting(ctx, "config_path", operation)?);
        let upstream = Self::setting(ctx, "upstream", operation)?.to_owned();
        let token = Self::host_token(ctx, host);

        let content = std::fs::read_to_string(&config_path).map_err(|cause| {
            error(
                AdapterErrorKind::Execution,
                ADAPTER_NAME,
                operation,
                format!(
                    "failed to read nginx config {}: {cause}",
                    config_path.display()
                ),
                None,
            )
        })?;
        let mut lines: Vec<String> = content.lines().map(ToOwned::to_owned).collect();

        let (index, prior) = locate(&lines, &upstream, &token).ok_or_else(|| {
            error(
                AdapterErrorKind::Execution,
                ADAPTER_NAME,
                operation,
                format!("host '{token}' not found in upstream '{upstream}'"),
                None,
            )
        })?;

        lines[index] = set_down(&lines[index], down);
        let mut rewritten = lines.join("\n");
        rewritten.push('\n');
        write_atomic(&config_path, &rewritten, operation)?;

        self.reload(operation).await?;
        Ok(prior)
    }

    /// `nginx -s reload`.
    async fn reload(&self, operation: &str) -> Result<(), AdapterError> {
        let args = [OsString::from("-s"), OsString::from("reload")];
        let captured =
            run_command(&self.program, &args, &[], None, ADAPTER_NAME, operation).await?;
        if captured.succeeded() {
            return Ok(());
        }
        Err(error(
            AdapterErrorKind::Execution,
            ADAPTER_NAME,
            operation,
            format!("nginx -s reload failed (exit {:?})", captured.code),
            captured.stderr_opt(),
        ))
    }
}

#[async_trait]
impl LbAdapter for NginxLb {
    async fn drain(&self, ctx: &AdapterCtx, host: &HostId) -> Result<LbMembership, AdapterError> {
        self.apply(ctx, host, "drain", true).await
    }

    async fn reattach(
        &self,
        ctx: &AdapterCtx,
        host: &HostId,
        prior: &LbMembership,
    ) -> Result<(), AdapterError> {
        // Restore the prior pool membership: back up if it was in the pool, leave
        // it down otherwise.
        let down = prior.state != LbState::InPool;
        self.apply(ctx, host, "reattach", down).await.map(|_| ())
    }
}

/// Find the `server` directive for `token` inside `upstream { … }`, returning its
/// line index and current membership.
fn locate(lines: &[String], upstream: &str, token: &str) -> Option<(usize, LbMembership)> {
    let mut in_block = false;
    for (index, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if !in_block {
            if let Some(rest) = line.strip_prefix("upstream") {
                let rest = rest.trim();
                if rest.starts_with('{') {
                    // `upstream {` with no name never matches a named upstream.
                } else if rest.split_whitespace().next() == Some(upstream) && line.contains('{') {
                    in_block = true;
                }
            }
            continue;
        }
        if line.starts_with('}') {
            break;
        }
        if let Some(rest) = line.strip_prefix("server") {
            let rest = rest.trim();
            let address = rest.split([' ', '\t', ';']).next().unwrap_or("");
            let host_part = address.split(':').next().unwrap_or("");
            if !address.is_empty() && host_part == token {
                let tokens = rest.split([' ', '\t', ';']);
                let down = tokens.clone().any(|t| t == "down");
                let weight = tokens
                    .clone()
                    .find_map(|t| t.strip_prefix("weight=").and_then(|w| w.parse().ok()));
                let state = if down {
                    LbState::Removed
                } else {
                    LbState::InPool
                };
                return Some((index, LbMembership { state, weight }));
            }
        }
    }
    None
}

/// Rewrite a `server` directive with or without the `down` flag, preserving
/// indentation and any other parameters (e.g. `weight=`).
fn set_down(raw: &str, down: bool) -> String {
    let indent_len = raw.len() - raw.trim_start().len();
    let (indent, rest) = raw.split_at(indent_len);
    let body = rest.trim_end();
    let body = body.strip_suffix(';').unwrap_or(body).trim_end();

    let mut tokens: Vec<&str> = body.split_whitespace().filter(|t| *t != "down").collect();
    if down {
        tokens.push("down");
    }
    format!("{indent}{};", tokens.join(" "))
}

/// Write `content` to `path` atomically, keeping a `<path>.bak` of the prior file.
fn write_atomic(path: &Path, content: &str, operation: &str) -> Result<(), AdapterError> {
    let to_err = |cause: std::io::Error| {
        error(
            AdapterErrorKind::Execution,
            ADAPTER_NAME,
            operation,
            format!("failed to update nginx config {}: {cause}", path.display()),
            None,
        )
    };
    let backup = PathBuf::from(format!("{}.bak", path.display()));
    let _ = std::fs::copy(path, &backup); // best-effort backup
    let tmp = PathBuf::from(format!("{}.fraisier-tmp", path.display()));
    std::fs::write(&tmp, content).map_err(to_err)?;
    std::fs::rename(&tmp, path).map_err(to_err)
}

#[cfg(test)]
mod tests {
    use super::{locate, set_down};
    use fraisier_core::adapter_axes::LbState;

    const UPSTREAM: &str = "\
http {
  upstream fraiseql_upstream {
    server web1.internal:8080 weight=5;
    server web2.internal:8080 down;
  }
}
";

    fn lines() -> Vec<String> {
        UPSTREAM.lines().map(ToOwned::to_owned).collect()
    }

    #[test]
    fn locate_reads_in_pool_membership_with_weight() {
        let (idx, membership) =
            locate(&lines(), "fraiseql_upstream", "web1.internal").expect("found");
        assert_eq!(idx, 2);
        assert_eq!(membership.state, LbState::InPool);
        assert_eq!(membership.weight, Some(5));
    }

    #[test]
    fn locate_reads_a_downed_host() {
        let (_idx, membership) =
            locate(&lines(), "fraiseql_upstream", "web2.internal").expect("found");
        assert_eq!(membership.state, LbState::Removed);
    }

    #[test]
    fn locate_ignores_other_upstreams_and_missing_hosts() {
        assert!(locate(&lines(), "other_upstream", "web1.internal").is_none());
        assert!(locate(&lines(), "fraiseql_upstream", "web9.internal").is_none());
    }

    #[test]
    fn set_down_adds_and_clears_preserving_weight_and_indent() {
        let line = "    server web1.internal:8080 weight=5;";
        let downed = set_down(line, true);
        assert_eq!(downed, "    server web1.internal:8080 weight=5 down;");
        assert_eq!(set_down(&downed, false), line);
    }

    #[test]
    fn set_down_is_idempotent() {
        let line = "  server x:1 down;";
        assert_eq!(set_down(line, true), "  server x:1 down;");
        assert_eq!(set_down(line, false), "  server x:1;");
    }
}

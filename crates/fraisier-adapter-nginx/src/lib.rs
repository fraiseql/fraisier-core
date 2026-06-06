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
use fraisier_adapter_support::{error, run_command, staging};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, HostId, LbAdapter,
    LbMembership, LbState, SwapToken, TrafficDirector, TrafficTarget,
};
use serde_json::Value;

/// The header marking the upstream-include files this adapter drops on the host,
/// so the full-CRUD list/uninstall rule can recognise them.
const TRAFFIC_MARKER: &str = "# fraisier-generated traffic-swap include (do not edit by hand)";

/// The basename of the symlink nginx `include`s; `switch_to` repoints it.
const ACTIVE_LINK: &str = "active.upstream";

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

impl NginxLb {
    /// The directory holding the per-target upstream includes + the active symlink.
    fn include_dir(ctx: &AdapterCtx, operation: &str) -> Result<PathBuf, AdapterError> {
        Ok(PathBuf::from(Self::setting(ctx, "include_dir", operation)?))
    }

    /// The per-target include file `<dir>/<target>.upstream.conf`.
    fn target_file(dir: &Path, target: &str) -> PathBuf {
        dir.join(format!("{target}.upstream.conf"))
    }

    /// The backend servers configured for `target` (the `[lb].targets` table),
    /// if any — `{ "green": ["10.0.0.2:8080", …] }`.
    fn target_servers(ctx: &AdapterCtx, target: &str) -> Option<Vec<String>> {
        ctx.settings
            .get("targets")
            .and_then(Value::as_object)
            .and_then(|targets| targets.get(target))
            .and_then(Value::as_array)
            .map(|servers| {
                servers
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
    }

    /// Render a MARKER-headed `upstream <name> { server …; }` include.
    fn render_upstream(name: &str, servers: &[String]) -> String {
        use std::fmt::Write as _;
        let mut out = format!("{TRAFFIC_MARKER}\nupstream {name} {{\n");
        for server in servers {
            let _ = writeln!(out, "    server {server};");
        }
        out.push_str("}\n");
        out
    }
}

#[async_trait]
impl TrafficDirector for NginxLb {
    async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
        Ok(AdapterDescription {
            name: ADAPTER_NAME.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: 1,
            capabilities: vec!["traffic_swap".to_owned()],
        })
    }

    async fn current_target(&self, ctx: &AdapterCtx) -> Result<TrafficTarget, AdapterError> {
        let dir = Self::include_dir(ctx, "current_target")?;
        let active = dir.join(ACTIVE_LINK);
        match staging::read_active_link(&active, ADAPTER_NAME, "current_target")? {
            Some(artifact) => {
                let name = artifact
                    .id
                    .strip_suffix(".upstream.conf")
                    .unwrap_or(&artifact.id);
                Ok(TrafficTarget::new(name))
            }
            None => Err(error(
                AdapterErrorKind::InvalidConfig,
                ADAPTER_NAME,
                "current_target",
                format!("no active upstream symlink at {}", active.display()),
                None,
            )),
        }
    }

    async fn switch_to(
        &self,
        ctx: &AdapterCtx,
        target: &TrafficTarget,
    ) -> Result<SwapToken, AdapterError> {
        let dir = Self::include_dir(ctx, "switch_to")?;
        let upstream = Self::setting(ctx, "upstream", "switch_to")?.to_owned();
        std::fs::create_dir_all(&dir).map_err(|cause| {
            error(
                AdapterErrorKind::Execution,
                ADAPTER_NAME,
                "switch_to",
                format!("failed to create include dir {}: {cause}", dir.display()),
                None,
            )
        })?;
        let target_file = Self::target_file(&dir, target.as_str());

        // Generate the target's include from its configured servers; if none are
        // configured the include must already exist (pre-staged by the operator).
        if let Some(servers) = Self::target_servers(ctx, target.as_str()) {
            let content = Self::render_upstream(&upstream, &servers);
            std::fs::write(&target_file, content).map_err(|cause| {
                error(
                    AdapterErrorKind::Execution,
                    ADAPTER_NAME,
                    "switch_to",
                    format!("failed to write {}: {cause}", target_file.display()),
                    None,
                )
            })?;
        } else if !target_file.exists() {
            return Err(error(
                AdapterErrorKind::InvalidConfig,
                ADAPTER_NAME,
                "switch_to",
                format!(
                    "no servers configured for target '{target}' and no include at {}",
                    target_file.display()
                ),
                None,
            ));
        }

        // Atomically repoint the active symlink, then reload nginx.
        staging::activate_symlink(
            &dir.join(ACTIVE_LINK),
            &target_file,
            ADAPTER_NAME,
            "switch_to",
        )?;
        self.reload("switch_to").await?;
        Ok(SwapToken {
            target: target.clone(),
        })
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

    mod traffic {
        use crate::{NginxLb, ACTIVE_LINK, TRAFFIC_MARKER};
        use fraisier_core::adapter_axes::{AdapterCtx, TrafficDirector, TrafficTarget};
        use serde_json::json;
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::Path;

        /// A fake `nginx` that records each `-s reload` to a log file and exits 0.
        fn fake_nginx(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
            let log = dir.join("reload.log");
            let script = dir.join("nginx");
            std::fs::write(
                &script,
                format!("#!/bin/sh\necho reload >> {}\nexit 0\n", log.display()),
            )
            .expect("write fake nginx");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            (script, log)
        }

        fn ctx(include_dir: &Path) -> AdapterCtx {
            let mut ctx = AdapterCtx::new("checkout", "production");
            ctx.settings.insert(
                "include_dir".to_owned(),
                json!(include_dir.display().to_string()),
            );
            ctx.settings
                .insert("upstream".to_owned(), json!("checkout_upstream"));
            ctx.settings.insert(
                "targets".to_owned(),
                json!({ "blue": ["127.0.0.1:9001"], "green": ["127.0.0.1:9002"] }),
            );
            ctx
        }

        #[tokio::test]
        async fn switch_generates_the_include_repoints_atomically_and_reloads() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let (nginx, log) = fake_nginx(tmp.path());
            let inc = tmp.path().join("includes");
            let lb = NginxLb::with_program(nginx);
            let ctx = ctx(&inc);

            // Swap to green.
            let token = lb
                .switch_to(&ctx, &TrafficTarget::new("green"))
                .await
                .expect("switch green");
            assert_eq!(token.target.as_str(), "green");

            // The green include was generated, MARKER-headed, with green's server.
            let green = std::fs::read_to_string(inc.join("green.upstream.conf")).expect("green");
            assert!(green.starts_with(TRAFFIC_MARKER), "marker header: {green}");
            assert!(green.contains("upstream checkout_upstream"), "{green}");
            assert!(green.contains("server 127.0.0.1:9002;"), "{green}");

            // The active symlink points at the green include; current_target agrees.
            let active = inc.join(ACTIVE_LINK);
            assert_eq!(
                std::fs::read_link(&active).unwrap().file_name().unwrap(),
                "green.upstream.conf"
            );
            assert_eq!(
                lb.current_target(&ctx).await.unwrap().as_str(),
                "green",
                "current_target round-trips the active symlink"
            );

            // Swap back to blue (the rollback primitive): symlink + target follow.
            lb.switch_to(&ctx, &TrafficTarget::new("blue"))
                .await
                .expect("switch blue");
            assert_eq!(lb.current_target(&ctx).await.unwrap().as_str(), "blue");
            assert_eq!(
                std::fs::read_link(&active).unwrap().file_name().unwrap(),
                "blue.upstream.conf"
            );

            // Idempotent: swapping to the already-live target is a clean no-error.
            lb.switch_to(&ctx, &TrafficTarget::new("blue"))
                .await
                .expect("idempotent");
            assert_eq!(lb.current_target(&ctx).await.unwrap().as_str(), "blue");

            // nginx was reloaded on each swap (3 swaps).
            let reloads = std::fs::read_to_string(&log).unwrap_or_default();
            assert_eq!(reloads.lines().count(), 3, "one reload per swap: {reloads}");
        }

        #[tokio::test]
        async fn current_target_errors_before_any_swap() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let (nginx, _log) = fake_nginx(tmp.path());
            let lb = NginxLb::with_program(nginx);
            let ctx = ctx(&tmp.path().join("includes"));
            assert!(
                lb.current_target(&ctx).await.is_err(),
                "no active upstream yet -> error, not a silent default"
            );
        }

        #[tokio::test]
        async fn describe_advertises_the_traffic_swap_capability() {
            let lb = NginxLb::with_program("/bin/true");
            let desc = lb.describe().await.expect("describe");
            assert!(desc.capabilities.iter().any(|c| c == "traffic_swap"));
        }
    }
}

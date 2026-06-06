//! Where an adapter's shell-out command runs: locally, or on a remote host over
//! `ssh`.
//!
//! The single-host adapters (`systemd`, `nginx`, `rc`, `docker-compose`) shell out
//! with [`run_command`](crate::run_command), which runs on the **local** machine.
//! A multi-host rollout has to run those same commands on each remote host. Rather
//! than teach every adapter about SSH, they dispatch through a [`Transport`]:
//!
//! - [`Transport::Local`] runs the command unchanged (the default — every existing
//!   single-host deploy keeps its exact behaviour).
//! - [`Transport::Ssh`] wraps the command as `ssh <dest> '<command>'`, resolving
//!   the target host's address **per call** from the adapter context
//!   ([`AdapterCtx::host`] / `settings["address"]`, which the multi-host composition
//!   fills in for each host). One SSH-mode adapter therefore serves the whole
//!   fleet.
//!
//! This is the "shell out in v1.0; a native SSH client is a later swap behind the
//! same seam" choice — it matches how the systemd adapter shells out to
//! `systemctl` rather than talking to D-Bus.
//!
//! # Secrets
//!
//! Migrations (the only axis that needs the database secret) run **once on the
//! orchestrator**, never per host, so they stay [`Transport::Local`] and the DSN
//! never reaches a remote `argv`. The per-host axes routed over SSH (service /
//! load-balancer) carry no secrets. Any `envs` passed to an SSH command become
//! `K=value` assignments on the remote command line, so callers must not send
//! secret values through it.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use fraisier_core::adapter_axes::{AdapterCtx, AdapterError, AdapterErrorKind};

use crate::{error, run_command, Captured};

/// The adapter identity used when the transport itself reports an error.
const TRANSPORT_ADAPTER: &str = "ssh";

/// The env var overriding which `ssh` binary the transport spawns (used by tests
/// to point at a fake).
const SSH_PROGRAM_ENV: &str = "FRAISIER_SSH_BIN";

/// How an adapter's shell-out command reaches its target host.
///
/// Build with [`Transport::Local`] (the default) or
/// [`Transport::ssh`](Transport::ssh); run a command with [`Transport::run`],
/// whose signature matches [`run_command`](crate::run_command) plus the
/// [`AdapterCtx`] (the SSH target is read from it).
#[derive(Debug, Clone, Default)]
pub enum Transport {
    /// Run the command on the local machine — unchanged single-host behaviour.
    #[default]
    Local,
    /// Run the command on a remote host over `ssh`.
    Ssh(SshTransport),
}

impl Transport {
    /// A transport that connects over `ssh` to the host named by each call's
    /// [`AdapterCtx`].
    #[must_use]
    pub const fn ssh(ssh: SshTransport) -> Self {
        Self::Ssh(ssh)
    }

    /// Run `program args` (with `envs`/`cwd`) on the transport's target, capturing
    /// its output. For [`Transport::Local`] this is exactly
    /// [`run_command`](crate::run_command); for [`Transport::Ssh`] the command is
    /// wrapped as a single remote shell command and `ssh` is spawned locally.
    ///
    /// # Errors
    /// [`AdapterError`] if the (local) process cannot be spawned, or — for SSH —
    /// if the context names no target host address. As with `run_command`, a
    /// process that runs and exits non-zero returns `Ok(Captured)`.
    // Reason: the signature deliberately mirrors `run_command` (its six args plus
    // the `AdapterCtx` the SSH target is read from) so adapters swap one call for
    // the other; bundling them would just relocate the arity.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        ctx: &AdapterCtx,
        program: &OsStr,
        args: &[OsString],
        envs: &[(OsString, OsString)],
        cwd: Option<&Path>,
        adapter: &str,
        operation: &str,
    ) -> Result<Captured, AdapterError> {
        match self {
            Self::Local => run_command(program, args, envs, cwd, adapter, operation).await,
            Self::Ssh(ssh) => {
                let destination = ssh.destination(ctx, operation)?;
                let remote = remote_command(program, args, envs, cwd);
                let ssh_args = ssh.ssh_argv(&destination, &remote);
                // `envs`/`cwd` are baked into the remote command, so the local
                // `ssh` process gets neither.
                run_command(&ssh.program, &ssh_args, &[], None, adapter, operation).await
            }
        }
    }
}

/// A configured SSH transport: the connection parameters shared by every host
/// (the per-host *address* comes from each call's [`AdapterCtx`]).
#[derive(Debug, Clone)]
pub struct SshTransport {
    program: OsString,
    user: Option<String>,
    port: Option<u16>,
    identity: Option<PathBuf>,
    options: Vec<String>,
}

impl Default for SshTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl SshTransport {
    /// A transport that invokes `ssh` (or `$FRAISIER_SSH_BIN` when set) with no
    /// user, port, identity, or extra options.
    #[must_use]
    pub fn new() -> Self {
        let program = std::env::var_os(SSH_PROGRAM_ENV)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("ssh"));
        Self {
            program,
            user: None,
            port: None,
            identity: None,
            options: Vec::new(),
        }
    }

    /// Set the login user (the `user@` in the destination).
    #[must_use]
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Set the SSH port (`-p`).
    #[must_use]
    pub const fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Set the identity file (`-i`).
    #[must_use]
    pub fn with_identity(mut self, identity: impl Into<PathBuf>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    /// Append extra `ssh -o` options (e.g. `ConnectTimeout=10`). Host-key
    /// checking is left to ssh's own configuration; fraisier never disables it.
    #[must_use]
    pub fn with_options(mut self, options: Vec<String>) -> Self {
        self.options = options;
        self
    }

    /// Override which `ssh` binary to spawn (tests point this at a fake).
    #[must_use]
    pub fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.program = program.into();
        self
    }

    /// Resolve the `ssh` destination (`[user@]address`) for this call. The address
    /// comes from `settings["address"]`, falling back to [`AdapterCtx::host`].
    fn destination(&self, ctx: &AdapterCtx, operation: &str) -> Result<String, AdapterError> {
        let address = ctx
            .settings
            .get("address")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| ctx.host.as_ref().map(|host| host.as_str().to_owned()))
            .ok_or_else(|| {
                error(
                    AdapterErrorKind::InvalidConfig,
                    TRANSPORT_ADAPTER,
                    operation,
                    "the ssh transport needs a target host address (set the host's \
                     [hosts].inventory address)"
                        .to_owned(),
                    None,
                )
            })?;
        Ok(match &self.user {
            Some(user) => format!("{user}@{address}"),
            None => address,
        })
    }

    /// The full `ssh` argv: a non-interactive default, the configured options /
    /// port / identity, the destination, then the remote command as one argument.
    fn ssh_argv(&self, destination: &str, remote: &str) -> Vec<OsString> {
        let mut argv: Vec<OsString> = Vec::new();
        // Never block a deploy on an interactive password/passphrase prompt.
        argv.push(OsString::from("-o"));
        argv.push(OsString::from("BatchMode=yes"));
        for option in &self.options {
            argv.push(OsString::from("-o"));
            argv.push(OsString::from(option));
        }
        if let Some(port) = self.port {
            argv.push(OsString::from("-p"));
            argv.push(OsString::from(port.to_string()));
        }
        if let Some(identity) = &self.identity {
            argv.push(OsString::from("-i"));
            argv.push(identity.clone().into_os_string());
        }
        argv.push(OsString::from(destination));
        argv.push(OsString::from(remote));
        argv
    }
}

/// Render `program args` (with optional `cwd` and `envs`) as a single shell
/// command for the remote login shell: `cd <dir> && K=v … <program> <args…>`,
/// each token quoted so spaces and metacharacters survive.
fn remote_command(
    program: &OsStr,
    args: &[OsString],
    envs: &[(OsString, OsString)],
    cwd: Option<&Path>,
) -> String {
    let mut command = String::new();
    if let Some(dir) = cwd {
        command.push_str("cd ");
        command.push_str(&shell_quote(&dir.to_string_lossy()));
        command.push_str(" && ");
    }
    for (key, value) in envs {
        command.push_str(&key.to_string_lossy());
        command.push('=');
        command.push_str(&shell_quote(&value.to_string_lossy()));
        command.push(' ');
    }
    command.push_str(&shell_quote(&program.to_string_lossy()));
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(&arg.to_string_lossy()));
    }
    command
}

/// Quote `value` for a POSIX shell: left bare when it is made only of safe
/// characters, otherwise single-quoted with embedded quotes escaped.
fn shell_quote(value: &str) -> String {
    // Characters safe to leave unquoted alongside ASCII alphanumerics.
    const SAFE: &[u8] = b"_-./:=@%+,";
    let safe = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || SAFE.contains(&byte));
    if safe {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::{remote_command, shell_quote, SshTransport, Transport};
    use fraisier_core::adapter_axes::{AdapterCtx, HostId};
    use serde_json::json;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn shell_quote_leaves_simple_tokens_bare_and_quotes_the_rest() {
        assert_eq!(shell_quote("systemctl"), "systemctl");
        assert_eq!(shell_quote("fraiseql.service"), "fraiseql.service");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn remote_command_renders_cwd_env_and_quoted_args() {
        let cmd = remote_command(
            OsString::from("systemctl").as_os_str(),
            &args(&["restart", "fraiseql.service"]),
            &[],
            None,
        );
        assert_eq!(cmd, "systemctl restart fraiseql.service");

        let with_ctx = remote_command(
            OsString::from("app").as_os_str(),
            &args(&["--flag", "two words"]),
            &[(OsString::from("KEY"), OsString::from("a value"))],
            Some(Path::new("/srv/app")),
        );
        assert_eq!(
            with_ctx,
            "cd /srv/app && KEY='a value' app --flag 'two words'"
        );
    }

    #[test]
    fn ssh_argv_carries_options_port_identity_destination_and_command() {
        let ssh = SshTransport::new()
            .with_port(2222)
            .with_identity("/keys/id")
            .with_options(vec!["StrictHostKeyChecking=no".to_owned()]);
        let argv: Vec<String> = ssh
            .ssh_argv("deploy@web1.internal", "systemctl restart x")
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "-p",
                "2222",
                "-i",
                "/keys/id",
                "deploy@web1.internal",
                "systemctl restart x",
            ]
        );
    }

    #[test]
    fn destination_prefers_the_address_setting_then_the_host() {
        let ssh = SshTransport::new().with_user("deploy");

        let mut from_setting = AdapterCtx::new("app", "prod");
        from_setting
            .settings
            .insert("address".to_owned(), json!("web1.internal"));
        assert_eq!(
            ssh.destination(&from_setting, "restart").expect("dest"),
            "deploy@web1.internal"
        );

        let mut from_host = AdapterCtx::new("app", "prod");
        from_host.host = Some(HostId::new("web2.internal"));
        assert_eq!(
            ssh.destination(&from_host, "restart").expect("dest"),
            "deploy@web2.internal"
        );

        let none = AdapterCtx::new("app", "prod");
        assert!(
            ssh.destination(&none, "restart").is_err(),
            "no address is an error"
        );
    }

    /// A fake `ssh` that prints its argv, one per line, so the test can assert the
    /// exact command the transport built. Returns the temp dir (kept alive) + path.
    fn fake_ssh() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake-ssh");
        std::fs::write(
            &path,
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
        )
        .expect("write fake ssh");
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
        (dir, path)
    }

    #[tokio::test]
    async fn ssh_transport_spawns_ssh_with_the_destination_and_remote_command() {
        let (_dir, fake) = fake_ssh();
        let transport = Transport::ssh(SshTransport::new().with_user("deploy").with_program(fake));

        let mut ctx = AdapterCtx::new("app", "prod");
        ctx.settings
            .insert("address".to_owned(), json!("web1.internal"));

        let captured = transport
            .run(
                &ctx,
                OsString::from("systemctl").as_os_str(),
                &args(&["restart", "fraiseql.service"]),
                &[],
                None,
                "systemd",
                "restart",
            )
            .await
            .expect("fake ssh spawns");
        assert!(captured.succeeded());
        let lines: Vec<&str> = captured.stdout.lines().collect();
        assert!(lines.contains(&"deploy@web1.internal"), "{lines:?}");
        assert!(
            lines.contains(&"systemctl restart fraiseql.service"),
            "remote command passed as one arg: {lines:?}"
        );
        assert!(lines.contains(&"BatchMode=yes"));
    }

    #[tokio::test]
    async fn local_transport_runs_the_command_directly() {
        let captured = Transport::Local
            .run(
                &AdapterCtx::new("app", "prod"),
                OsString::from("printf").as_os_str(),
                &args(&["hello"]),
                &[],
                None,
                "test",
                "printf",
            )
            .await
            .expect("printf spawns");
        assert!(captured.succeeded());
        assert_eq!(captured.stdout, "hello");
    }
}

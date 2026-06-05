//! Where an IPC adapter subprocess runs: locally, or on a remote host over `ssh`.
//!
//! `fraisier-ipc` speaks `Content-Length`-framed JSON-RPC over a child's piped
//! stdio. *Which* child — a local adapter binary, or `ssh <host> -- <adapter>` —
//! is the only thing that differs between running an adapter in-process on the
//! orchestrator and running it on the target host; the framing and the JSON-RPC
//! protocol are byte-identical either way. A [`Launcher`] captures that choice:
//!
//! - [`Launcher::Local`] spawns the adapter binary directly (the default — every
//!   existing migration deploy keeps its exact behaviour).
//! - [`Launcher::Ssh`] spawns `ssh [opts] <dest> <adapter>` so the adapter runs on
//!   the remote host and does its filesystem/HTTP work *there*; the JSON-RPC bytes
//!   flow through ssh's stdio transparently. The destination is resolved **per
//!   call** from the [`AdapterCtx`] (`settings["address"]`, falling back to
//!   [`AdapterCtx::host`]) exactly like the shell-out transport, so one Ssh-mode
//!   adapter serves the whole fleet. OpenSSH **`ControlMaster`** multiplexing
//!   amortises connection setup across the many per-host calls a deploy makes.
//!
//! # Secrets
//!
//! The Ssh launcher forwards **no** environment to the remote argv: secret values
//! never cross `ssh` (PRD review Decision 5). The only axis that needs a secret —
//! migration — runs [`Launcher::Local`] on the orchestrator, where its `envs` are
//! applied to the child as usual.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use fraisier_core::adapter_axes::{AdapterCtx, AdapterError, AdapterErrorKind};
use serde_json::Value;
use tokio::process::Command;

/// The env var overriding which `ssh` binary the launcher spawns (tests point this
/// at a fake).
const SSH_PROGRAM_ENV: &str = "FRAISIER_SSH_BIN";

/// The adapter identity used when the launcher itself reports an error.
const TRANSPORT_ADAPTER: &str = "ssh";

/// Default `ControlPersist`: keep the multiplexed master open this many seconds
/// after the last client closes, so a deploy's burst of per-host calls reuses one
/// connection.
const DEFAULT_CONTROL_PERSIST: &str = "60";

/// How an IPC adapter subprocess is launched.
///
/// Build with [`Launcher::Local`] (the default) or
/// [`Launcher::ssh`](Launcher::ssh).
#[derive(Debug, Clone, Default)]
pub enum Launcher {
    /// Spawn the adapter binary on the local machine — unchanged behaviour.
    #[default]
    Local,
    /// Spawn the adapter on a remote host over `ssh`.
    Ssh(SshLauncher),
}

impl Launcher {
    /// A launcher that runs the adapter on the host named by each call's
    /// [`AdapterCtx`], over `ssh`.
    #[must_use]
    pub const fn ssh(ssh: SshLauncher) -> Self {
        Self::Ssh(ssh)
    }

    /// Build the [`Command`] that launches `program args`.
    ///
    /// For [`Self::Local`] this is the adapter binary with `envs` applied; for
    /// [`Self::Ssh`] it is `ssh` reaching the host named by `ctx` (with **no** env
    /// on the remote argv — secrets stay local).
    pub(crate) fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        envs: &BTreeMap<OsString, OsString>,
        ctx: Option<&AdapterCtx>,
        operation: &str,
    ) -> Result<Command, AdapterError> {
        match self {
            Self::Local => {
                let mut command = Command::new(program);
                command.args(args).envs(envs);
                Ok(command)
            }
            Self::Ssh(ssh) => ssh.command(program, args, ctx, operation),
        }
    }

    /// The program actually spawned (for diagnostics): the adapter when
    /// [`Self::Local`], `ssh` when [`Self::Ssh`].
    pub(crate) fn spawn_program<'a>(&'a self, program: &'a OsStr) -> &'a OsStr {
        match self {
            Self::Local => program,
            Self::Ssh(ssh) => ssh.program.as_os_str(),
        }
    }
}

/// A configured `ssh` launcher: the connection parameters shared by every host
/// (the per-host *address* comes from each call's [`AdapterCtx`]).
#[derive(Debug, Clone)]
pub struct SshLauncher {
    program: OsString,
    user: Option<String>,
    port: Option<u16>,
    identity: Option<PathBuf>,
    options: Vec<String>,
    control_dir: Option<PathBuf>,
    control_persist: String,
}

impl Default for SshLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl SshLauncher {
    /// A launcher that invokes `ssh` (or `$FRAISIER_SSH_BIN` when set) with no
    /// user, port, identity, extra options, or connection multiplexing.
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
            control_dir: None,
            control_persist: DEFAULT_CONTROL_PERSIST.to_owned(),
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

    /// Append extra `ssh -o` options (e.g. `StrictHostKeyChecking=no`).
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

    /// Enable OpenSSH `ControlMaster` multiplexing, storing the per-connection
    /// control socket under `dir` (which the caller must ensure exists). Without
    /// this, every call opens a fresh connection — fine for a one-shot, costly for
    /// a deploy's burst of per-host calls.
    #[must_use]
    pub fn with_control_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.control_dir = Some(dir.into());
        self
    }

    /// Override `ControlPersist` (seconds the master stays open after the last
    /// client; default `60`).
    #[must_use]
    pub fn with_control_persist(mut self, persist: impl Into<String>) -> Self {
        self.control_persist = persist.into();
        self
    }

    /// Resolve the `ssh` destination (`[user@]address`) for this call. The address
    /// comes from `settings["address"]`, falling back to [`AdapterCtx::host`] —
    /// the same precedence the shell-out transport uses.
    fn destination(
        &self,
        ctx: Option<&AdapterCtx>,
        operation: &str,
    ) -> Result<String, AdapterError> {
        let ctx = ctx.ok_or_else(|| no_address(operation))?;
        let address = ctx
            .settings
            .get("address")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| ctx.host.as_ref().map(|host| host.as_str().to_owned()))
            .ok_or_else(|| no_address(operation))?;
        Ok(match &self.user {
            Some(user) => format!("{user}@{address}"),
            None => address,
        })
    }

    /// Build the `ssh` [`Command`] reaching the `ctx`-named host and running
    /// `program args` there.
    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        ctx: Option<&AdapterCtx>,
        operation: &str,
    ) -> Result<Command, AdapterError> {
        let destination = self.destination(ctx, operation)?;
        let remote = remote_command(program, args);
        let mut command = Command::new(&self.program);
        command.args(self.ssh_argv(&destination, &remote));
        Ok(command)
    }

    /// The full `ssh` argv: a non-interactive default, the optional `ControlMaster`
    /// multiplexing, the configured options / port / identity, the destination,
    /// then the remote command as one argument.
    fn ssh_argv(&self, destination: &str, remote: &str) -> Vec<OsString> {
        let mut argv: Vec<OsString> = Vec::new();
        // Never block a deploy on an interactive password/passphrase prompt.
        push_option(&mut argv, "BatchMode=yes");
        if let Some(dir) = &self.control_dir {
            // `%C` is a hash of (localhost, host, port, user): one socket per host.
            push_option(&mut argv, "ControlMaster=auto");
            let mut control_path = OsString::from("ControlPath=");
            control_path.push(dir.join("cm-%C").as_os_str());
            argv.push(OsString::from("-o"));
            argv.push(control_path);
            push_option(
                &mut argv,
                &format!("ControlPersist={}", self.control_persist),
            );
        }
        for option in &self.options {
            push_option(&mut argv, option);
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

/// Push a `-o <option>` pair onto an `ssh` argv.
fn push_option(argv: &mut Vec<OsString>, option: &str) {
    argv.push(OsString::from("-o"));
    argv.push(OsString::from(option));
}

/// The "no target host address" error for an Ssh-launched call.
fn no_address(operation: &str) -> AdapterError {
    AdapterError {
        adapter: Some(TRANSPORT_ADAPTER.to_owned()),
        operation: Some(operation.to_owned()),
        ..AdapterError::new(
            AdapterErrorKind::InvalidConfig,
            "the ssh launcher needs a target host address (set the host's \
             [hosts].inventory address)"
                .to_owned(),
        )
    }
}

/// Render `program args` as a single shell command for the remote login shell,
/// each token quoted so spaces and metacharacters survive. Adapter binaries take
/// no shell-sensitive arguments, but quoting keeps the launcher honest.
fn remote_command(program: &OsStr, args: &[OsString]) -> String {
    let mut command = shell_quote(&program.to_string_lossy());
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(&arg.to_string_lossy()));
    }
    command
}

/// Quote `value` for a POSIX shell: left bare when made only of safe characters,
/// otherwise single-quoted with embedded quotes escaped.
fn shell_quote(value: &str) -> String {
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
    use super::{remote_command, shell_quote, Launcher, SshLauncher};
    use fraisier_core::adapter_axes::{AdapterCtx, HostId};
    use serde_json::json;
    use std::ffi::OsString;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn shell_quote_leaves_simple_tokens_bare_and_quotes_the_rest() {
        assert_eq!(
            shell_quote("fraisier-adapter-release"),
            "fraisier-adapter-release"
        );
        assert_eq!(shell_quote("/usr/local/bin/x"), "/usr/local/bin/x");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn remote_command_quotes_program_and_args() {
        assert_eq!(
            remote_command(OsString::from("fraisier-adapter-release").as_os_str(), &[]),
            "fraisier-adapter-release"
        );
        assert_eq!(
            remote_command(
                OsString::from("/opt/a dapter").as_os_str(),
                &args(&["--flag", "two words"])
            ),
            "'/opt/a dapter' --flag 'two words'"
        );
    }

    #[test]
    fn ssh_argv_carries_controlmaster_options_port_identity_dest_and_command() {
        let ssh = SshLauncher::new()
            .with_user("deploy")
            .with_port(2222)
            .with_identity("/keys/id")
            .with_options(vec!["StrictHostKeyChecking=no".to_owned()])
            .with_control_dir("/run/fraisier-cm")
            .with_control_persist("90");
        let argv: Vec<String> = ssh
            .ssh_argv("deploy@web1.internal", "fraisier-adapter-release")
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=/run/fraisier-cm/cm-%C",
                "-o",
                "ControlPersist=90",
                "-o",
                "StrictHostKeyChecking=no",
                "-p",
                "2222",
                "-i",
                "/keys/id",
                "deploy@web1.internal",
                "fraisier-adapter-release",
            ]
        );
    }

    #[test]
    fn ssh_argv_omits_controlmaster_without_a_control_dir() {
        let argv: Vec<String> = SshLauncher::new()
            .ssh_argv("web1", "x")
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !argv.iter().any(|a| a.contains("ControlMaster")),
            "{argv:?}"
        );
        assert_eq!(argv.first().map(String::as_str), Some("-o"));
    }

    #[test]
    fn destination_prefers_the_address_setting_then_the_host() {
        let ssh = SshLauncher::new().with_user("deploy");

        let mut from_setting = AdapterCtx::new("app", "prod");
        from_setting
            .settings
            .insert("address".to_owned(), json!("web1.internal"));
        assert_eq!(
            ssh.destination(Some(&from_setting), "stage").expect("dest"),
            "deploy@web1.internal"
        );

        let mut from_host = AdapterCtx::new("app", "prod");
        from_host.host = Some(HostId::new("web2.internal"));
        assert_eq!(
            ssh.destination(Some(&from_host), "stage").expect("dest"),
            "deploy@web2.internal"
        );

        assert!(
            ssh.destination(None, "stage").is_err(),
            "no ctx (hence no address) is an error"
        );
        assert!(
            ssh.destination(Some(&AdapterCtx::new("app", "prod")), "stage")
                .is_err(),
            "a ctx with no address is an error"
        );
    }

    #[test]
    fn local_launcher_is_the_default() {
        assert!(matches!(Launcher::default(), Launcher::Local));
    }
}

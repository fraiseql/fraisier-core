//! The systemd units a refresh stops, starts, and checks for the outbound flag.

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;

use async_trait::async_trait;
use fraisier_config::DeployConfig;

/// The systemd units a refresh stops and starts.
#[async_trait]
pub(crate) trait Units: Send + Sync {
    /// Whether `unit` is running.
    ///
    /// # Errors
    /// `systemctl` could not be run.
    async fn is_active(&self, unit: &str) -> Result<bool, String>;

    /// Stop `unit`.
    ///
    /// # Errors
    /// `systemctl stop` failed.
    async fn stop(&self, unit: &str) -> Result<(), String>;

    /// Start `unit`.
    ///
    /// # Errors
    /// `systemctl start` failed.
    async fn start(&self, unit: &str) -> Result<(), String>;

    /// The environment files `unit` loads (`EnvironmentFiles=`).
    ///
    /// # Errors
    /// `systemctl show` failed.
    async fn environment_files(&self, unit: &str) -> Result<Vec<PathBuf>, String>;

    /// The units a timer starts (`Triggers=`).
    ///
    /// # Errors
    /// `systemctl show` failed.
    async fn triggers(&self, unit: &str) -> Result<Vec<String>, String>;

    /// `unit`'s main process, or 0 when none runs (`MainPID=`).
    ///
    /// # Errors
    /// `systemctl show` failed or printed no number.
    async fn main_pid(&self, unit: &str) -> Result<u32, String>;

    /// The group `unit` runs as: its `Group=`, else its `User=`'s primary
    /// group, else root's.
    ///
    /// # Errors
    /// The group could not be resolved.
    async fn group(&self, unit: &str) -> Result<u32, String>;

    /// Process `pid`'s environment, as `/proc/<pid>/environ` holds it.
    ///
    /// # Errors
    /// It could not be read: only root or the process's own user may.
    async fn environ(&self, pid: u32) -> Result<Vec<u8>, String>;
}

/// [`Units`] over `systemctl` (or `$FRAISIER_SYSTEMCTL_BIN`), on this host.
pub(crate) struct Systemctl {
    program: OsString,
    user: bool,
}

impl Systemctl {
    /// Drive the user manager when `user`, else the system manager.
    pub(crate) fn new(user: bool) -> Self {
        let program = std::env::var_os("FRAISIER_SYSTEMCTL_BIN")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("systemctl"));
        Self { program, user }
    }

    async fn run(&self, args: &[&str]) -> Result<std::process::Output, String> {
        let mut command = tokio::process::Command::new(&self.program);
        fraisier_core::child_env::scrub(command.as_std_mut());
        if self.user {
            command.arg("--user");
        }
        command
            .args(args)
            .output()
            .await
            .map_err(|error| format!("could not run systemctl: {error}"))
    }

    /// `systemctl show -p <property> --value <unit>`, trimmed.
    async fn property(&self, unit: &str, property: &str) -> Result<String, String> {
        let out = self.run(&["show", "-p", property, "--value", unit]).await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            Err(format!(
                "systemctl show -p {property} {unit}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    async fn act(&self, verb: &str, unit: &str) -> Result<(), String> {
        let out = self.run(&[verb, unit]).await?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "systemctl {verb} {unit}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

#[async_trait]
impl Units for Systemctl {
    async fn is_active(&self, unit: &str) -> Result<bool, String> {
        let out = self.run(&["is-active", unit]).await?;
        Ok(String::from_utf8_lossy(&out.stdout).trim() == "active")
    }

    async fn stop(&self, unit: &str) -> Result<(), String> {
        self.act("stop", unit).await
    }

    async fn start(&self, unit: &str) -> Result<(), String> {
        self.act("start", unit).await
    }

    async fn environment_files(&self, unit: &str) -> Result<Vec<PathBuf>, String> {
        let out = self.run(&["show", "-p", "EnvironmentFiles", unit]).await?;
        if !out.status.success() {
            return Err(format!(
                "systemctl show -p EnvironmentFiles {unit}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(parse_environment_files(&String::from_utf8_lossy(
            &out.stdout,
        )))
    }

    async fn triggers(&self, unit: &str) -> Result<Vec<String>, String> {
        Ok(self
            .property(unit, "Triggers")
            .await?
            .split_whitespace()
            .map(str::to_owned)
            .collect())
    }

    async fn main_pid(&self, unit: &str) -> Result<u32, String> {
        let pid = self.property(unit, "MainPID").await?;
        pid.parse()
            .map_err(|_| format!("{unit} has an unreadable MainPID: {pid:?}"))
    }

    async fn group(&self, unit: &str) -> Result<u32, String> {
        let group = self.property(unit, "Group").await?;
        if !group.is_empty() {
            return id_of("getent", &["group", &group], 2, &group).await;
        }
        let user = self.property(unit, "User").await?;
        if user.is_empty() {
            return Ok(0);
        }
        id_of("id", &["-g", "--", &user], 0, &user).await
    }

    async fn environ(&self, pid: u32) -> Result<Vec<u8>, String> {
        let path = format!("/proc/{pid}/environ");
        std::fs::read(&path).map_err(|error| format!("cannot read {path}: {error}"))
    }
}

/// The number in `:`-separated `field` of what `program args` prints: a gid
/// from `getent group` (field 2) or `id -g` (field 0).
async fn id_of(program: &str, args: &[&str], field: usize, name: &str) -> Result<u32, String> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|error| format!("could not run {program}: {error}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .trim()
        .split(':')
        .nth(field)
        .and_then(|id| id.parse().ok())
        .filter(|_| out.status.success())
        .ok_or_else(|| format!("cannot resolve the group of {name}"))
}

/// The files `systemctl show -p EnvironmentFiles <unit>` lists, one
/// `EnvironmentFiles=<path> (ignore_errors=…)` line each.
fn parse_environment_files(show: &str) -> Vec<PathBuf> {
    show.lines()
        .filter_map(|line| line.strip_prefix("EnvironmentFiles="))
        .map(|value| {
            value
                .rsplit_once(" (ignore_errors=")
                .map_or(value, |(path, _)| path)
        })
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Whether `environ` (`NAME=value` entries, NUL separated) sets `name` to
/// exactly `1`.
pub(super) fn environ_sets(environ: &[u8], name: &str) -> bool {
    let wanted = format!("{name}=1");
    environ
        .split(|byte| *byte == 0)
        .any(|entry| entry == wanted.as_bytes())
}

/// The uid this process runs as.
pub(super) fn current_uid() -> u32 {
    std::fs::metadata("/proc/self").map_or(u32::MAX, |meta| meta.uid())
}

/// Whether `unit` is a timer.
pub(super) fn is_timer(unit: &str) -> bool {
    unit.rsplit_once('.')
        .is_some_and(|(_, kind)| kind == "timer")
}

/// The units a refresh stops, in order: the timers first, so nothing is
/// started again behind it, then the other services, then the `[service]`
/// unit. They start again in the reverse order.
pub(super) fn units_to_stop(config: &DeployConfig, service: &str) -> Vec<String> {
    let extra = config
        .refresh
        .as_ref()
        .map(|refresh| refresh.units.as_slice())
        .unwrap_or_default();
    let timers = extra.iter().filter(|unit| is_timer(unit));
    let services = extra.iter().filter(|unit| !is_timer(unit));
    timers
        .chain(services)
        .cloned()
        .chain(std::iter::once(service.to_owned()))
        .collect()
}

/// The `[service]` unit and whether it runs under the user manager, or why a
/// refresh cannot drive this config's service.
pub(super) fn systemd_service(
    config: &DeployConfig,
) -> std::result::Result<(String, bool), &'static str> {
    if config.hosts.is_some() {
        return Err(
            "a refresh runs on the one host it is invoked on, and this config \
                    deploys to [hosts]",
        );
    }
    let service = config.service.as_ref().filter(|service| {
        service.adapter.as_deref() == Some("systemd")
            && service.unit.as_deref().is_some_and(|unit| !unit.is_empty())
    });
    service.map_or(
        Err(
            "a refresh stops and starts the service with systemd, and this config has no \
             [service] adapter = \"systemd\" with a unit",
        ),
        |service| {
            Ok((
                service.unit.clone().unwrap_or_default(),
                service.user == Some(true),
            ))
        },
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use super::{environ_sets, parse_environment_files, Systemctl, Units};
    use crate::test_env::block_on;

    /// A `systemctl` whose units are real `sleep` processes, started with the
    /// variables of `<dir>/flag.env`, so `/proc/<pid>/environ` is real.
    fn fake_systemctl(dir: &Path) -> PathBuf {
        let path = dir.join("systemctl");
        let run = dir.display();
        std::fs::write(
            &path,
            format!(
                "#!/bin/bash\n\
                 R={run}\n\
                 case \"$1\" in\n\
                 is-active) [ -f $R/$2.pid ] && echo active || echo inactive ;;\n\
                 stop) kill $(cat $R/$2.pid); rm -f $R/$2.pid ;;\n\
                 start) ( set -a; . $R/flag.env; exec sleep 30 ) >/dev/null 2>&1 & echo $! > $R/$2.pid ;;\n\
                 show) case \"$3\" in\n\
                   EnvironmentFiles) echo \"EnvironmentFiles=$R/flag.env (ignore_errors=yes)\" ;;\n\
                   MainPID) cat $R/$5.pid 2>/dev/null || echo 0 ;;\n\
                   Triggers) echo \"${{5%.timer}}.service\" ;;\n\
                   Group) echo ;;\n\
                   User) echo ;;\n\
                 esac ;;\n\
                 esac\n"
            ),
        )
        .expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[test]
    fn the_real_seam_reads_a_started_process_s_environment() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("flag.env"), "FRAISIER_REFRESHED=1\n").expect("flag");
        let units = Systemctl {
            program: fake_systemctl(dir.path()).into(),
            user: false,
        };
        block_on(async {
            units.start("app.service").await.expect("start");
            assert!(units.is_active("app.service").await.expect("is-active"));
            let files = units.environment_files("app.service").await.expect("files");
            assert_eq!(files, [dir.path().join("flag.env")]);
            assert_eq!(
                units.triggers("beat.timer").await.expect("triggers"),
                ["beat.service"]
            );
            assert_eq!(units.group("app.service").await.expect("group"), 0);
            let pid = units.main_pid("app.service").await.expect("pid");
            assert_ne!(pid, 0);
            let environ = units.environ(pid).await.expect("environ");
            assert!(environ_sets(&environ, "FRAISIER_REFRESHED"));
            units.stop("app.service").await.expect("stop");
            assert_eq!(units.main_pid("app.service").await.expect("pid"), 0);
            assert!(!units.is_active("app.service").await.expect("is-active"));
            // pid 1's environment is root's alone.
            if super::current_uid() != 0 {
                let error = units.environ(1).await.expect_err("unreadable");
                assert!(error.contains("/proc/1/environ"), "{error}");
            }
        });
    }

    #[test]
    fn every_environment_file_line_is_read() {
        let show = "EnvironmentFiles=/etc/checkout/staging.env (ignore_errors=yes)\n\
                    EnvironmentFiles=/etc/checkout/staging.refreshed.env (ignore_errors=yes)\n";
        assert_eq!(
            parse_environment_files(show),
            vec![
                PathBuf::from("/etc/checkout/staging.env"),
                PathBuf::from("/etc/checkout/staging.refreshed.env")
            ]
        );
        assert_eq!(
            parse_environment_files("EnvironmentFiles=\n"),
            Vec::<PathBuf>::new()
        );
        assert_eq!(parse_environment_files(""), Vec::<PathBuf>::new());
    }

    #[test]
    fn the_flag_must_be_exactly_one() {
        let environ = b"PATH=/usr/bin\0FRAISIER_REFRESHED=1\0HOME=/\0";
        assert!(environ_sets(environ, "FRAISIER_REFRESHED"));
        assert!(!environ_sets(
            b"FRAISIER_REFRESHED=0\0",
            "FRAISIER_REFRESHED"
        ));
        assert!(!environ_sets(
            b"FRAISIER_REFRESHED_X=1\0",
            "FRAISIER_REFRESHED"
        ));
        assert!(!environ_sets(
            b"X_FRAISIER_REFRESHED=1\0",
            "FRAISIER_REFRESHED"
        ));
        assert!(!environ_sets(b"", "FRAISIER_REFRESHED"));
    }
}

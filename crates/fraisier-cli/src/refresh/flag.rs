//! The outbound flag: the file a refresh writes so every unit of the
//! environment starts with `<VAR>=1`, and the two checks that it does.

use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use super::units::{environ_sets, is_timer, Units};

/// `<etc>/<fraise>/<environment>.refreshed.env`.
pub(super) fn flag_path(etc: &Path, fraise: &str, environment: &str) -> PathBuf {
    etc.join(fraise)
        .join(format!("{environment}.refreshed.env"))
}

/// Why the directory holding the flag file would let someone other than root
/// or the operator replace or delete it, or does not exist.
pub(super) fn dir_problem(dir: &Path, operator_uid: u32) -> Option<String> {
    let shown = dir.display();
    let meta = match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => meta,
        Ok(_) => return Some(format!("{shown} is not a directory")),
        Err(error) => {
            return Some(format!(
                "cannot use {shown} for the outbound flag: {error}. Create it: \
                 install -d -o root -m 0755 {shown}"
            ))
        }
    };
    if meta.mode() & 0o022 != 0 {
        return Some(format!(
            "{shown} is group- or world-writable (mode {:o}), so the app could delete the \
             outbound flag",
            meta.mode() & 0o7777
        ));
    }
    (meta.uid() != 0 && meta.uid() != operator_uid).then(|| {
        format!(
            "{shown} is owned by uid {}, neither root nor the operator, so the outbound flag \
             there is not under the refresh's control",
            meta.uid()
        )
    })
}

/// D18 check 1: every unit loads `flag`. A timer is checked through the
/// units it triggers, which are what run.
///
/// # Errors
/// The units that do not load it, and how to fix that.
pub(super) async fn loaded_by_every_unit(
    units: &dyn Units,
    names: &[String],
    flag: &Path,
) -> Result<(), String> {
    let mut checked: Vec<String> = Vec::new();
    for name in names {
        let run = if is_timer(name) {
            units.triggers(name).await?
        } else {
            vec![name.clone()]
        };
        for unit in run {
            if !checked.contains(&unit) {
                checked.push(unit);
            }
        }
    }
    let mut missing = Vec::new();
    for unit in &checked {
        if !units
            .environment_files(unit)
            .await?
            .iter()
            .any(|f| f == flag)
        {
            missing.push(unit.clone());
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} {} not load the outbound flag file, so after a refresh {} would start with its \
         integrations on. Add `EnvironmentFile=-{}` to each (`fraisier scaffold` generates \
         it for the [service] unit and a drop-in for each of [refresh].units), then \
         `systemctl daemon-reload`",
        missing.join(", "),
        if missing.len() == 1 { "does" } else { "do" },
        if missing.len() == 1 { "it" } else { "they" },
        flag.display()
    ))
}

/// Write `<var>=1` to `flag`, mode 0640, group `gid`, owned by this process
/// (root, on a host): the app's group can read it and not change it.
///
/// # Errors
/// The file was not written in full.
pub(super) fn write(flag: &Path, var: &str, gid: u32) -> Result<(), String> {
    let shown = flag.display();
    let name = flag
        .file_name()
        .map_or_else(Default::default, |name| name.to_string_lossy());
    let partial = flag.with_file_name(format!(".{name}.partial"));
    let written = (|| -> std::io::Result<()> {
        let _ = std::fs::remove_file(&partial);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial)?;
        writeln!(file, "{var}=1")?;
        file.sync_all()?;
        std::os::unix::fs::chown(&partial, None, Some(gid))?;
        std::fs::set_permissions(
            &partial,
            std::os::unix::fs::PermissionsExt::from_mode(0o640),
        )?;
        std::fs::rename(&partial, flag)
    })();
    written.map_err(|error| {
        let _ = std::fs::remove_file(&partial);
        format!("could not write the outbound flag {shown}: {error}")
    })
}

/// Remove `flag`. `false` when it was not there.
///
/// # Errors
/// It is there and could not be removed.
pub(super) fn clear(flag: &Path) -> Result<bool, String> {
    match std::fs::remove_file(flag) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("could not remove {}: {error}", flag.display())),
    }
}

/// D18 check 2: every started service runs with `<var>=1`, read from its main
/// process's environment. Returns each unit's pid.
///
/// # Errors
/// A unit has no main process, its environment cannot be read (never taken as
/// a pass), or it lacks the flag.
pub(super) async fn prove(
    units: &dyn Units,
    started: &[String],
    var: &str,
) -> Result<Vec<(String, u32)>, String> {
    let mut proof = Vec::new();
    for unit in started.iter().filter(|unit| !is_timer(unit)) {
        let pid = units.main_pid(unit).await?;
        if pid == 0 {
            return Err(format!(
                "{unit} has no running main process, so its outbound flag cannot be proven"
            ));
        }
        let environ = units.environ(pid).await.map_err(|error| {
            format!(
                "{error}: the outbound flag of {unit} cannot be proven. Run the refresh as \
                 root (or as {unit}'s user)"
            )
        })?;
        if !environ_sets(&environ, var) {
            return Err(format!(
                "{unit} (pid {pid}) started without {var}=1 in its environment"
            ));
        }
        proof.push((unit.clone(), pid));
    }
    Ok(proof)
}

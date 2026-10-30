//! A refresh's sanitize step: the project's own SQL file or executable, run
//! inside the refreshed database as the app role, before the service starts.

use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::PgConn;

/// What ran, and the SHA-256 of the exact bytes that ran (a `.sql` file) or
/// of the executable as it was read just before it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sanitized {
    /// The step's path.
    pub path: PathBuf,
    /// `"sql"` or `"executable"`.
    pub kind: &'static str,
    /// Lower-case hex SHA-256.
    pub sha256: String,
}

/// Run the sanitize step at `path` against `conn`'s database.
///
/// A `.sql` file is read once, hashed, and those same bytes are piped to
/// `psql -X -v ON_ERROR_STOP=1 -1`: one transaction, nothing committed on an
/// error. Anything else runs as an executable with no arguments. Either way
/// the child's environment is `PATH` and `conn`'s `PG*` variables, nothing
/// else; a step that is group- or world-writable, or owned by someone other
/// than root or `operator_uid`, is refused before it runs.
///
/// # Errors
/// Why the step was refused or failed.
pub async fn run(path: &Path, conn: &PgConn, operator_uid: u32) -> Result<Sanitized, String> {
    run_with(Path::new("psql"), path, conn, operator_uid).await
}

/// Why the step at `path` may not run, checked before anything is stopped:
/// it is missing, or someone other than root or `operator_uid` could change it.
///
/// # Errors
/// The reason.
pub fn check(path: &Path, operator_uid: u32) -> Result<(), String> {
    let shown = path.display();
    let meta = std::fs::metadata(path).map_err(|error| format!("cannot read {shown}: {error}"))?;
    ownership_problem(meta.uid(), meta.mode(), operator_uid).map_or(Ok(()), |problem| {
        Err(format!("refusing to run {shown}: {problem}"))
    })
}

/// [`run`] with the `psql` that runs a `.sql` step supplied by the caller.
async fn run_with(
    psql: &Path,
    path: &Path,
    conn: &PgConn,
    operator_uid: u32,
) -> Result<Sanitized, String> {
    let shown = path.display();
    check(path, operator_uid)?;
    let bytes = std::fs::read(path).map_err(|error| format!("cannot read {shown}: {error}"))?;
    let sha256 = sha256_hex(&bytes);
    let is_sql = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"));
    let (mut command, kind) = if is_sql {
        let mut command = scrubbed(psql, conn);
        command
            .args(["-X", "-q", "-v", "ON_ERROR_STOP=1", "-1", "-f", "-"])
            .stdin(Stdio::piped());
        (command, "sql")
    } else {
        (scrubbed(path, conn), "executable")
    };
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run {shown}: {error}"))?;
    if is_sql {
        let mut stdin = child.stdin.take().ok_or("psql has no stdin")?;
        stdin
            .write_all(&bytes)
            .await
            .map_err(|error| format!("could not feed {shown} to psql: {error}"))?;
        drop(stdin);
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|error| format!("could not run {shown}: {error}"))?;
    if !out.status.success() {
        return Err(format!(
            "{shown} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(Sanitized {
        path: path.to_owned(),
        kind,
        sha256,
    })
}

/// Why a step owned by `uid` with `mode` must not run as the refresh's step:
/// someone other than root or the operator could have changed it.
fn ownership_problem(uid: u32, mode: u32, operator_uid: u32) -> Option<String> {
    if mode & 0o022 != 0 {
        return Some(format!(
            "it is group- or world-writable (mode {:o})",
            mode & 0o7777
        ));
    }
    (uid != 0 && uid != operator_uid).then(|| {
        format!("it is owned by uid {uid}, neither root nor the operator (uid {operator_uid})")
    })
}

/// `program` with an environment of `PATH` and `conn`'s `PG*` variables only.
fn scrubbed(program: &Path, conn: &PgConn) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command.envs(conn.pg_env());
    command
}

/// Lower-case hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;

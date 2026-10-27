//! The failure record a scheduled backup, prune or drill leaves for its alert.
//!
//! A unit that fails cannot notify reliably from inside the failing process: it
//! may have died before it could. So the failing command (run with
//! `--failure-record <unit>`) writes a small, already-redacted record keyed by
//! its systemd invocation ID, and the unit's `OnFailure=` alert
//! (`fraisier notify`) picks the record up by `$MONITOR_INVOCATION_ID`, sends
//! it, and deletes it. Only the alert notifies, so a failure is reported once.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The largest record written or read. A reason too long for it is truncated.
pub(crate) const MAX_BYTES: usize = 4096;

/// What a failed run tells its alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FailureRecord {
    /// The payload event, e.g. `scheduled-backup-failed`.
    pub(crate) event: String,
    /// `[deploy].name`, when the config could be read.
    pub(crate) fraise: Option<String>,
    /// `[deploy].environment`, when the config could be read.
    pub(crate) environment: Option<String>,
    /// The failing unit, as `%n` names it.
    pub(crate) unit: String,
    /// The failing activation's systemd invocation ID.
    pub(crate) invocation_id: String,
    /// When the record was written, in seconds since the Unix epoch.
    pub(crate) at_unix: u64,
    /// Why the run failed, with credentials removed.
    pub(crate) reason: String,
}

/// Where records live: `$XDG_RUNTIME_DIR/fraisier/failures` for a user unit,
/// `/run/fraisier/failures` for a system one. The failing units declare
/// `RuntimeDirectory=fraisier/failures`, which is the same path.
pub(crate) fn dir(xdg_runtime_dir: Option<&OsStr>) -> PathBuf {
    xdg_runtime_dir
        .filter(|dir| !dir.is_empty())
        .map_or_else(|| PathBuf::from("/run"), PathBuf::from)
        .join("fraisier/failures")
}

/// Whether `id` has the shape of a systemd invocation ID (32 lowercase hex
/// digits). Anything else is refused, so an ID can never name a path.
pub(crate) fn is_invocation_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Write `record` into `dir` as `<invocation_id>.json`: mode 0600, complete
/// before it is visible, at most [`MAX_BYTES`], and with its reason redacted
/// whatever the caller passed.
///
/// # Errors
/// An I/O error, or `InvalidInput` when the invocation ID is malformed.
pub(crate) fn write(dir: &Path, record: &FailureRecord) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    if !is_invocation_id(&record.invocation_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a systemd invocation ID",
        ));
    }
    let body = bounded(FailureRecord {
        reason: fraisier_saga::redact::credentials(&record.reason),
        ..record.clone()
    })?;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", record.invocation_id));
    let partial = dir.join(format!(".{}.json.partial", record.invocation_id));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&partial)
        .and_then(|mut file| file.write_all(&body))
        .and_then(|()| std::fs::rename(&partial, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written.map(|()| path)
}

/// `record` as JSON of at most [`MAX_BYTES`], cutting the reason short if needed.
fn bounded(mut record: FailureRecord) -> std::io::Result<Vec<u8>> {
    const MARK: &str = "… (truncated)";
    loop {
        let body = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
        if body.len() <= MAX_BYTES {
            return Ok(body);
        }
        let reason = record.reason.strip_suffix(MARK).unwrap_or(&record.reason);
        // Escaping can make the JSON longer than the text, so cut by the excess
        // and try again rather than computing the fit.
        let mut keep = reason
            .len()
            .saturating_sub(body.len() - MAX_BYTES + MARK.len());
        while !reason.is_char_boundary(keep) {
            keep -= 1;
        }
        if keep == 0 && reason.is_empty() {
            return Err(std::io::Error::other(
                "a failure record without a reason is too large",
            ));
        }
        record.reason = format!("{}{MARK}", &reason[..keep]);
    }
}

/// The record for invocation `id` in `dir`, and its path. `None` when the ID is
/// malformed or the record is absent, oversized or unreadable.
pub(crate) fn read(dir: &Path, id: &str) -> Option<(FailureRecord, PathBuf)> {
    if !is_invocation_id(id) {
        return None;
    }
    let path = dir.join(format!("{id}.json"));
    let size = std::fs::metadata(&path).ok()?.len();
    if size > MAX_BYTES as u64 {
        return None;
    }
    let record = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    Some((record, path))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::{dir, is_invocation_id, read, write, FailureRecord, MAX_BYTES};

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn record(reason: &str) -> FailureRecord {
        FailureRecord {
            event: "scheduled-backup-failed".to_owned(),
            fraise: Some("checkout".to_owned()),
            environment: Some("production".to_owned()),
            unit: "fraisier-checkout-production-scheduled.service".to_owned(),
            invocation_id: ID.to_owned(),
            at_unix: 1_790_000_000,
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn a_system_unit_records_under_run() {
        assert_eq!(dir(None), std::path::Path::new("/run/fraisier/failures"));
    }

    #[test]
    fn a_user_unit_records_under_its_runtime_dir() {
        assert_eq!(
            dir(Some(std::ffi::OsStr::new("/run/user/1000"))),
            std::path::Path::new("/run/user/1000/fraisier/failures")
        );
    }

    #[test]
    fn only_a_systemd_invocation_id_is_accepted() {
        assert!(is_invocation_id(ID));
        for bad in [
            "",
            "../../etc/passwd",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
        ] {
            assert!(!is_invocation_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_record_round_trips_with_mode_0600() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write(tmp.path(), &record("pg_dump exited 1")).expect("write");
        assert_eq!(path, tmp.path().join(format!("{ID}.json")));
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let (read_back, at) = read(tmp.path(), ID).expect("read");
        assert_eq!(read_back, record("pg_dump exited 1"));
        assert_eq!(at, path);
    }

    #[test]
    fn the_directory_is_created_when_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let nested = tmp.path().join("fraisier/failures");
        write(&nested, &record("x")).expect("write");
        assert!(read(&nested, ID).is_some());
    }

    #[test]
    fn the_reason_is_redacted_on_write() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write(
            tmp.path(),
            &record("connect postgres://app:s3cr3t-pw@db/shop failed"),
        )
        .expect("write");
        let on_disk = std::fs::read_to_string(path).expect("read");
        assert!(!on_disk.contains("s3cr3t-pw"), "{on_disk}");
    }

    #[test]
    fn a_long_reason_is_truncated_to_fit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write(tmp.path(), &record(&"é".repeat(10_000))).expect("write");
        let size = std::fs::metadata(&path).expect("stat").len();
        assert!(size <= MAX_BYTES as u64, "{size}");
        let (read_back, _) = read(tmp.path(), ID).expect("read");
        assert!(
            read_back.reason.ends_with("(truncated)"),
            "{}",
            read_back.reason
        );
    }

    #[test]
    fn no_partial_file_is_left_behind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write(tmp.path(), &record("x")).expect("write");
        let names: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("ls")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(format!("{ID}.json"))]);
    }

    #[test]
    fn a_malformed_id_is_refused_on_write_and_read() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut bad = record("x");
        bad.invocation_id = "../escape".to_owned();
        assert!(write(tmp.path(), &bad).is_err());
        assert!(read(tmp.path(), "../escape").is_none());
    }

    #[test]
    fn an_absent_or_oversized_record_reads_as_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(read(tmp.path(), ID).is_none());
        std::fs::write(
            tmp.path().join(format!("{ID}.json")),
            "x".repeat(MAX_BYTES + 1),
        )
        .expect("write");
        assert!(read(tmp.path(), ID).is_none());
    }
}

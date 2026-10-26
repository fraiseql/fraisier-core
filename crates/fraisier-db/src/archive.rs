//! Timestamped backup archives in a directory.
//!
//! A scheduled backup runs unattended, so every run must produce a new archive
//! and no run may damage an earlier one. Each archive is named
//! `<fraise>-<environment>-<yyyymmddThhmmssZ>.pgdump`, in UTC at second
//! resolution. The name is the only ordering: "latest" is the newest timestamp in
//! a name, never the newest mtime, because a copy or a `touch` changes mtime
//! without changing what the archive holds.
//!
//! [`write_archive`] never leaves a damaged archive under a final name. `pg_dump`
//! writes into `.<name>.partial` in the same directory, the archive is checked,
//! and only then is it renamed into place. A dump that fails or is rejected is
//! deleted, so it can neither replace a good archive nor become the next run's
//! size baseline.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{DbError, PgConn};

/// The extension every archive carries.
const EXTENSION: &str = ".pgdump";

/// A UTC timestamp at second resolution, rendered as `yyyymmddThhmmssZ`.
///
/// The rendering is fixed-width, so ordering the strings orders the instants.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcStamp(String);

impl UtcStamp {
    /// The stamp for `time`, truncated to the second.
    ///
    /// A time before the Unix epoch renders as the epoch.
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        let seconds = time
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
        let (year, month, day) = civil_from_days(days);
        let of_day = seconds % 86_400;
        Self(format!(
            "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
            of_day / 3_600,
            of_day % 3_600 / 60,
            of_day % 60
        ))
    }

    /// Parse a rendered stamp, rejecting anything that is not a real UTC instant
    /// in exactly the `yyyymmddThhmmssZ` shape.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 16 || bytes[8] != b'T' || bytes[15] != b'Z' {
            return None;
        }
        let number = |range: std::ops::Range<usize>| -> Option<u32> {
            let digits = &text[range];
            digits
                .bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| digits.parse().ok())
                .flatten()
        };
        let (year, month, day) = (number(0..4)?, number(4..6)?, number(6..8)?);
        let (hour, minute, second) = (number(9..11)?, number(11..13)?, number(13..15)?);
        let real = (1..=12).contains(&month)
            && (1..=days_in_month(year, month)).contains(&day)
            && hour < 24
            && minute < 60
            && second < 60;
        real.then(|| Self(text.to_owned()))
    }

    /// The rendered `yyyymmddThhmmssZ` form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One archive found in a backup directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// The archive's path (the directory joined with its file name).
    pub path: PathBuf,
    /// The timestamp its name carries.
    pub stamp: UtcStamp,
}

/// The file name of the archive for `fraise`/`environment` taken at `stamp`.
#[must_use]
pub fn archive_file_name(fraise: &str, environment: &str, stamp: &UtcStamp) -> String {
    format!("{fraise}-{environment}-{}{EXTENSION}", stamp.as_str())
}

/// The stamp in `file_name` if it is an archive of `fraise`/`environment`.
///
/// Partial files, other deployables' archives and foreign files all yield `None`.
#[must_use]
pub fn parse_archive_file_name(
    fraise: &str,
    environment: &str,
    file_name: &str,
) -> Option<UtcStamp> {
    let stamp = file_name
        .strip_prefix(fraise)?
        .strip_prefix('-')?
        .strip_prefix(environment)?
        .strip_prefix('-')?
        .strip_suffix(EXTENSION)?;
    UtcStamp::parse(stamp)
}

/// Every archive of `fraise`/`environment` in `dir`, oldest first.
///
/// # Errors
/// Reading `dir` fails (including when it does not exist).
pub fn list_archives(dir: &Path, fraise: &str, environment: &str) -> std::io::Result<Vec<Archive>> {
    let mut archives = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !entry.file_type()?.is_file() {
            continue;
        }
        if let Some(stamp) = parse_archive_file_name(fraise, environment, &file_name) {
            archives.push(Archive {
                path: entry.path(),
                stamp,
            });
        }
    }
    archives.sort_by(|left, right| left.stamp.cmp(&right.stamp));
    Ok(archives)
}

/// The newest archive of `fraise`/`environment` in `dir`, by the stamp in its name.
///
/// # Errors
/// Reading `dir` fails (including when it does not exist).
pub fn latest_archive(
    dir: &Path,
    fraise: &str,
    environment: &str,
) -> std::io::Result<Option<Archive>> {
    Ok(list_archives(dir, fraise, environment)?.pop())
}

/// The programs [`write_archive`] runs. [`Default`] finds both on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveTools {
    /// The `pg_dump` that writes the archive.
    pub pg_dump: PathBuf,
    /// The `pg_restore` whose `--list` proves the archive reads back.
    pub pg_restore: PathBuf,
}

impl Default for ArchiveTools {
    fn default() -> Self {
        Self {
            pg_dump: PathBuf::from("pg_dump"),
            pg_restore: PathBuf::from("pg_restore"),
        }
    }
}

/// An archive [`write_archive`] put in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenArchive {
    /// The archive under its final name.
    pub archive: Archive,
    /// Its size in bytes.
    pub bytes: u64,
    /// Checks that could not run. The archive was kept, but these say what is
    /// unproven about it.
    pub warnings: Vec<String>,
}

/// Why [`write_archive`] put no archive in place. In every case the archives
/// already in the directory are untouched.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// An archive with this run's name already exists. Two runs in the same
    /// second refuse rather than overwrite.
    #[error("{} already exists; a backup never overwrites an archive", .0.display())]
    Exists(PathBuf),
    /// Another run is writing this run's name right now (its partial file is
    /// present). That file is left alone.
    #[error("{} is present: another backup is writing this archive", .0.display())]
    InProgress(PathBuf),
    /// `pg_dump` exited non-zero.
    #[error("pg_dump failed: {stderr}")]
    DumpFailed {
        /// `pg_dump`'s standard error, trimmed.
        stderr: String,
    },
    /// `pg_restore --list` could not read the archive back.
    #[error("pg_restore --list rejected the new archive: {stderr}")]
    Unreadable {
        /// `pg_restore`'s standard error, trimmed.
        stderr: String,
    },
    /// The new archive is under half the size of the previous one, which is how
    /// a truncated dump looks.
    #[error(
        "the new archive is {bytes} bytes, under half of the previous {previous_bytes} bytes \
         ({}); refusing it as a likely truncated dump",
        .previous.display()
    )]
    Shrunk {
        /// The new archive's size.
        bytes: u64,
        /// The previous archive's size.
        previous_bytes: u64,
        /// The previous archive.
        previous: PathBuf,
    },
    /// `pg_dump` could not be launched.
    #[error(transparent)]
    Spawn(#[from] DbError),
    /// A filesystem operation on the backup directory failed.
    #[error("{action} {}: {source}", .path.display())]
    Io {
        /// What was being done.
        action: &'static str,
        /// The path it was done to.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

/// Dump `conn` into a new archive of `fraise`/`environment` in `dir`, named for
/// `stamp`.
///
/// The archive appears under its final name only once all of these hold:
/// - `pg_dump` exited 0;
/// - `pg_restore --list` reads it (if `pg_restore` cannot be run, the archive is
///   kept with a warning instead);
/// - it is at least half the size of the newest archive already in `dir`.
///
/// # Errors
/// See [`ArchiveError`]. On every error the partial file this call created is
/// removed and earlier archives are untouched.
pub async fn write_archive(
    conn: &PgConn,
    dir: &Path,
    fraise: &str,
    environment: &str,
    stamp: &UtcStamp,
    tools: &ArchiveTools,
) -> Result<WrittenArchive, ArchiveError> {
    let name = archive_file_name(fraise, environment, stamp);
    let target = dir.join(&name);
    let partial = Partial::claim(dir.join(format!(".{name}.partial")))?;
    // Checked after the claim: a run that renamed this name into place before we
    // claimed is visible now, and one still writing it holds the claim.
    if target.exists() {
        return Err(ArchiveError::Exists(target));
    }
    let previous = latest_archive(dir, fraise, environment).map_err(|source| ArchiveError::Io {
        action: "listing",
        path: dir.to_owned(),
        source,
    })?;

    let mut dump = Command::new(&tools.pg_dump);
    dump.envs(conn.pg_env());
    dump.arg("-Fc").arg("-w").arg("-f").arg(partial.path());
    let outcome = crate::run(dump).await?;
    if !outcome.succeeded() {
        return Err(ArchiveError::DumpFailed {
            stderr: outcome.stderr.trim().to_owned(),
        });
    }

    let mut warnings = Vec::new();
    let mut list = Command::new(&tools.pg_restore);
    list.arg("--list").arg(partial.path());
    match crate::run(list).await {
        Ok(outcome) if outcome.succeeded() => {}
        Ok(outcome) => {
            return Err(ArchiveError::Unreadable {
                stderr: outcome.stderr.trim().to_owned(),
            })
        }
        Err(error) => warnings.push(format!(
            "the archive was not verified with `pg_restore --list`: {error}"
        )),
    }

    let bytes = partial.len()?;
    if let Some(previous) = previous {
        let previous_bytes = std::fs::metadata(&previous.path)
            .map_err(|source| ArchiveError::Io {
                action: "reading the size of",
                path: previous.path.clone(),
                source,
            })?
            .len();
        if bytes.saturating_mul(2) < previous_bytes {
            return Err(ArchiveError::Shrunk {
                bytes,
                previous_bytes,
                previous: previous.path,
            });
        }
    }

    partial.persist(&target, dir)?;
    Ok(WrittenArchive {
        archive: Archive {
            path: target,
            stamp: stamp.clone(),
        },
        bytes,
        warnings,
    })
}

/// A partial file this run created, removed on drop unless persisted.
struct Partial {
    path: PathBuf,
    persisted: bool,
}

impl Partial {
    /// Create `path` exclusively, so two runs can never write the same archive.
    fn claim(path: PathBuf) -> Result<Self, ArchiveError> {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(Self {
                path,
                persisted: false,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(ArchiveError::InProgress(path))
            }
            Err(source) => Err(ArchiveError::Io {
                action: "creating",
                path,
                source,
            }),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn len(&self) -> Result<u64, ArchiveError> {
        std::fs::metadata(&self.path)
            .map(|metadata| metadata.len())
            .map_err(|source| ArchiveError::Io {
                action: "reading the size of",
                path: self.path.clone(),
                source,
            })
    }

    /// Flush the archive to disk and rename it to `target`, then flush `dir` so
    /// the rename itself survives a crash.
    fn persist(mut self, target: &Path, dir: &Path) -> Result<(), ArchiveError> {
        let io = |action, path: &Path| {
            let path = path.to_owned();
            move |source| ArchiveError::Io {
                action,
                path,
                source,
            }
        };
        std::fs::File::open(&self.path)
            .and_then(|file| file.sync_all())
            .map_err(io("flushing", &self.path))?;
        std::fs::rename(&self.path, target).map_err(io("renaming into place", target))?;
        self.persisted = true;
        std::fs::File::open(dir)
            .and_then(|file| file.sync_all())
            .map_err(io("flushing", dir))
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

const fn is_leap(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

const fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The proleptic-Gregorian `(year, month, day)` of `days` since 1970-01-01.
///
/// Howard Hinnant's `civil_from_days`, kept here so this crate needs no date
/// library for one fixed-format rendering.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::{Duration, UNIX_EPOCH};

    use super::{
        archive_file_name, latest_archive, list_archives, parse_archive_file_name, write_archive,
        ArchiveError, ArchiveTools, UtcStamp,
    };
    use crate::PgConn;

    fn stamp(text: &str) -> UtcStamp {
        UtcStamp::parse(text).expect("a valid stamp")
    }

    #[test]
    fn a_system_time_renders_as_its_utc_second() {
        // 2026-10-26T03:15:42.9Z
        let time = UNIX_EPOCH + Duration::from_millis(1_792_984_542_900);
        assert_eq!(
            UtcStamp::from_system_time(time).as_str(),
            "20261026T031542Z"
        );
    }

    #[test]
    fn a_leap_day_renders_correctly() {
        // 2028-02-29T00:00:00Z
        let time = UNIX_EPOCH + Duration::from_hours(509_832);
        assert_eq!(
            UtcStamp::from_system_time(time).as_str(),
            "20280229T000000Z"
        );
    }

    #[test]
    fn the_epoch_renders_as_the_epoch() {
        assert_eq!(
            UtcStamp::from_system_time(UNIX_EPOCH).as_str(),
            "19700101T000000Z"
        );
    }

    #[test]
    fn a_stamp_round_trips_through_its_rendering() {
        assert_eq!(stamp("20261026T031542Z").as_str(), "20261026T031542Z");
    }

    #[test]
    fn malformed_stamps_are_rejected() {
        for text in [
            "",
            "20261026T031542",
            "20261026t031542Z",
            "2026102T0031542Z",
            "20261326T031542Z",
            "20261000T031542Z",
            "20260230T031542Z",
            "20261026T241542Z",
            "20261026T036042Z",
            "20261026T031560Z",
            "２0261026T031542Z",
            "20261026T031542Z ",
        ] {
            assert_eq!(UtcStamp::parse(text), None, "{text:?} should be rejected");
        }
    }

    #[test]
    fn a_name_round_trips_through_parse() {
        let name = archive_file_name("app", "prod", &stamp("20261026T031542Z"));
        assert_eq!(name, "app-prod-20261026T031542Z.pgdump");
        assert_eq!(
            parse_archive_file_name("app", "prod", &name),
            Some(stamp("20261026T031542Z"))
        );
    }

    #[test]
    fn foreign_partial_and_other_deployables_names_do_not_parse() {
        for name in [
            ".app-prod-20261026T031542Z.pgdump.partial",
            "app-prod-20261026T031542Z.pgdump.partial",
            "app-prod.pgdump",
            "app-prod-20261026T031542Z.dump",
            "app-staging-20261026T031542Z.pgdump",
            "api-prod-20261026T031542Z.pgdump",
            "app-prod-eu-20261026T031542Z.pgdump",
            "notes.txt",
        ] {
            assert_eq!(
                parse_archive_file_name("app", "prod", name),
                None,
                "{name:?} should not parse"
            );
        }
    }

    #[test]
    fn listing_keeps_only_this_deployables_archives_oldest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in [
            "app-prod-20261026T031542Z.pgdump",
            "app-prod-20261024T031542Z.pgdump",
            ".app-prod-20261027T031542Z.pgdump.partial",
            "app-staging-20261028T031542Z.pgdump",
            "app-prod.pgdump",
            "README",
        ] {
            std::fs::write(dir.path().join(name), b"x").expect("write");
        }
        let names: Vec<String> = list_archives(dir.path(), "app", "prod")
            .expect("list")
            .into_iter()
            .map(|archive| archive.stamp.as_str().to_owned())
            .collect();
        assert_eq!(names, ["20261024T031542Z", "20261026T031542Z"]);
    }

    #[test]
    fn latest_is_by_the_name_not_by_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let newer = dir.path().join("app-prod-20261026T031542Z.pgdump");
        let older = dir.path().join("app-prod-20261024T031542Z.pgdump");
        std::fs::write(&newer, b"x").expect("write");
        // Written second, so its mtime is the newest.
        std::fs::write(&older, b"x").expect("write");
        let latest = latest_archive(dir.path(), "app", "prod")
            .expect("list")
            .expect("an archive");
        assert_eq!(latest.path, newer);
    }

    #[test]
    fn an_empty_directory_has_no_latest() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            latest_archive(dir.path(), "app", "prod").expect("list"),
            None
        );
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(list_archives(&dir.path().join("absent"), "app", "prod").is_err());
    }

    /// Linux's `ETXTBSY`: exec'ing a file some process still holds open for
    /// writing.
    const ETXTBSY: i32 = 26;

    /// The fake `pg_dump` and `pg_restore`, written once per test process.
    ///
    /// Writing an executable and exec'ing it at once races with a sibling test's
    /// `fork` (the child inherits the open write fd, and exec fails with
    /// `ETXTBSY`), so the scripts are never rewritten. Each test's scenario lives
    /// in a `scenario/` directory beside its backup directory, which the scripts
    /// find from the archive path they are handed.
    fn fakes() -> &'static ArchiveTools {
        static TOOLS: OnceLock<ArchiveTools> = OnceLock::new();
        TOOLS.get_or_init(|| {
            let dir =
                std::env::temp_dir().join(format!("fraisier-db-fakes-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("mkdir fakes");
            // pg_dump -Fc -w -f <out>: write ./dump_bytes zero bytes, exit ./dump_exit.
            let pg_dump = write_fake(
                &dir.join("pg_dump"),
                "out=\"$4\"\n\
                 scenario=\"$(dirname \"$out\")/../scenario\"\n\
                 head -c \"$(cat \"$scenario/dump_bytes\")\" /dev/zero > \"$out\"\n\
                 echo 'pg_dump: error: connection lost' >&2\n\
                 exit \"$(cat \"$scenario/dump_exit\")\"\n",
            );
            // pg_restore --list <archive>: exit ./list_exit.
            let pg_restore = write_fake(
                &dir.join("pg_restore"),
                "scenario=\"$(dirname \"$2\")/../scenario\"\n\
                 code=\"$(cat \"$scenario/list_exit\")\"\n\
                 [ \"$code\" = 0 ] || echo 'pg_restore: error: not a valid archive' >&2\n\
                 exit \"$code\"\n",
            );
            ArchiveTools {
                pg_dump,
                pg_restore,
            }
        })
    }

    fn write_fake(path: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        // `--probe` (the burn-in below) exits before the body touches any file.
        std::fs::write(
            path,
            format!("#!/bin/sh\n[ \"$1\" = --probe ] && exit 0\n{body}"),
        )
        .expect("write fake");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod fake");
        // Burn the ETXTBSY window left by any sibling fork.
        for _ in 0..200 {
            match std::process::Command::new(path).arg("--probe").output() {
                Ok(_) => return path.to_owned(),
                Err(error) if error.raw_os_error() == Some(ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fake is not executable: {error}"),
            }
        }
        panic!("fake stayed ETXTBSY");
    }

    /// A backup directory and the scenario the fakes read.
    struct Scene {
        root: tempfile::TempDir,
    }

    impl Scene {
        fn new(dump_bytes: u64, dump_exit: i32, list_exit: i32) -> Self {
            let root = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir(root.path().join("backups")).expect("mkdir backups");
            let scenario = root.path().join("scenario");
            std::fs::create_dir(&scenario).expect("mkdir scenario");
            std::fs::write(scenario.join("dump_bytes"), dump_bytes.to_string()).expect("write");
            std::fs::write(scenario.join("dump_exit"), dump_exit.to_string()).expect("write");
            std::fs::write(scenario.join("list_exit"), list_exit.to_string()).expect("write");
            Self { root }
        }

        fn dir(&self) -> PathBuf {
            self.root.path().join("backups")
        }

        /// Put an earlier archive of `bytes` bytes in place, returning its path.
        fn previous(&self, bytes: usize) -> PathBuf {
            let path = self.dir().join("app-prod-20261025T031500Z.pgdump");
            std::fs::write(&path, vec![b'g'; bytes]).expect("write previous");
            path
        }

        /// Every file name in the backup directory, sorted.
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.dir())
                .expect("read dir")
                .map(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .into_string()
                        .expect("utf-8")
                })
                .collect();
            names.sort();
            names
        }

        async fn run(
            &self,
            stamp: &str,
            tools: &ArchiveTools,
        ) -> Result<super::WrittenArchive, ArchiveError> {
            let conn = PgConn::parse("postgres://app:s3cret@localhost/app").expect("dsn");
            write_archive(
                &conn,
                &self.dir(),
                "app",
                "prod",
                &UtcStamp::parse(stamp).expect("stamp"),
                tools,
            )
            .await
        }
    }

    const PREVIOUS: &str = "app-prod-20261025T031500Z.pgdump";
    const NEW: &str = "app-prod-20261026T031500Z.pgdump";

    #[tokio::test]
    async fn a_successful_run_leaves_exactly_one_new_archive() {
        let scene = Scene::new(1_000, 0, 0);
        scene.previous(1_000);
        let written = scene
            .run("20261026T031500Z", fakes())
            .await
            .expect("backup");
        assert_eq!(scene.names(), [PREVIOUS, NEW]);
        assert_eq!(
            (written.archive.path, written.bytes),
            (scene.dir().join(NEW), 1_000)
        );
        assert_eq!(written.warnings, Vec::<String>::new());
    }

    #[tokio::test]
    async fn two_runs_a_day_apart_leave_two_archives() {
        let scene = Scene::new(1_000, 0, 0);
        scene
            .run("20261025T031500Z", fakes())
            .await
            .expect("first run");
        scene
            .run("20261026T031500Z", fakes())
            .await
            .expect("second run");
        assert_eq!(scene.names(), [PREVIOUS, NEW]);
    }

    #[tokio::test]
    async fn a_failed_dump_leaves_the_previous_archive_and_no_partial() {
        let scene = Scene::new(400, 1, 0);
        let previous = scene.previous(1_000);
        let error = scene
            .run("20261026T031500Z", fakes())
            .await
            .expect_err("dump fails");
        assert!(
            matches!(error, ArchiveError::DumpFailed { ref stderr } if stderr.contains("connection lost")),
            "{error:?}"
        );
        assert_eq!(scene.names(), [PREVIOUS]);
        assert_eq!(std::fs::read(previous).expect("read"), vec![b'g'; 1_000]);
    }

    #[tokio::test]
    async fn a_second_run_in_the_same_second_refuses_and_overwrites_nothing() {
        let scene = Scene::new(1_000, 0, 0);
        scene
            .run("20261026T031500Z", fakes())
            .await
            .expect("first run");
        std::fs::write(scene.root.path().join("scenario/dump_bytes"), "10").expect("write");
        let error = scene
            .run("20261026T031500Z", fakes())
            .await
            .expect_err("same name");
        assert!(
            matches!(error, ArchiveError::Exists(ref path) if path.ends_with(NEW)),
            "{error:?}"
        );
        assert_eq!(
            std::fs::metadata(scene.dir().join(NEW))
                .expect("stat")
                .len(),
            1_000
        );
        assert_eq!(scene.names(), [NEW]);
    }

    #[tokio::test]
    async fn another_runs_partial_file_is_refused_and_left_alone() {
        let scene = Scene::new(1_000, 0, 0);
        let partial = scene.dir().join(format!(".{NEW}.partial"));
        std::fs::write(&partial, b"in flight").expect("write partial");
        let error = scene
            .run("20261026T031500Z", fakes())
            .await
            .expect_err("in progress");
        assert!(
            matches!(error, ArchiveError::InProgress(ref path) if *path == partial),
            "{error:?}"
        );
        assert_eq!(std::fs::read(&partial).expect("read"), b"in flight");
        assert_eq!(scene.names(), [format!(".{NEW}.partial")]);
    }

    #[tokio::test]
    async fn an_archive_pg_restore_cannot_list_is_rejected_and_removed() {
        let scene = Scene::new(1_000, 0, 1);
        let previous = scene.previous(1_000);
        let error = scene
            .run("20261026T031500Z", fakes())
            .await
            .expect_err("unreadable");
        assert!(
            matches!(error, ArchiveError::Unreadable { ref stderr } if stderr.contains("not a valid archive")),
            "{error:?}"
        );
        assert_eq!(scene.names(), [PREVIOUS]);
        assert_eq!(std::fs::read(previous).expect("read"), vec![b'g'; 1_000]);
    }

    #[tokio::test]
    async fn a_dump_under_half_the_previous_is_rejected_and_not_left_on_disk() {
        let scene = Scene::new(400, 0, 0);
        let previous = scene.previous(1_000);
        let error = scene
            .run("20261026T031500Z", fakes())
            .await
            .expect_err("shrunk");
        assert!(
            matches!(error, ArchiveError::Shrunk { bytes: 400, previous_bytes: 1_000, previous: ref path } if *path == previous),
            "{error:?}"
        );
        assert_eq!(scene.names(), [PREVIOUS]);
    }

    #[tokio::test]
    async fn a_dump_of_exactly_half_the_previous_is_kept() {
        let scene = Scene::new(500, 0, 0);
        scene.previous(1_000);
        scene
            .run("20261026T031500Z", fakes())
            .await
            .expect("half is not under half");
        assert_eq!(scene.names(), [PREVIOUS, NEW]);
    }

    #[tokio::test]
    async fn without_pg_restore_the_archive_is_kept_with_a_warning() {
        let scene = Scene::new(1_000, 0, 1);
        let tools = ArchiveTools {
            pg_restore: scene.root.path().join("no-such-pg_restore"),
            ..fakes().clone()
        };
        let written = scene.run("20261026T031500Z", &tools).await.expect("backup");
        assert_eq!(scene.names(), [NEW]);
        assert_eq!(written.warnings.len(), 1, "{:?}", written.warnings);
        assert!(
            written.warnings[0].contains("pg_restore"),
            "{:?}",
            written.warnings
        );
    }

    #[tokio::test]
    async fn a_missing_pg_dump_is_a_spawn_error_and_leaves_no_partial() {
        let scene = Scene::new(1_000, 0, 0);
        let tools = ArchiveTools {
            pg_dump: scene.root.path().join("no-such-pg_dump"),
            ..fakes().clone()
        };
        let error = scene
            .run("20261026T031500Z", &tools)
            .await
            .expect_err("no pg_dump");
        assert!(matches!(error, ArchiveError::Spawn(_)), "{error:?}");
        assert_eq!(scene.names(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn the_password_reaches_pg_dump_through_the_environment_only() {
        let scene = Scene::new(1_000, 0, 0);
        let tools = ArchiveTools {
            pg_dump: write_fake(
                &scene.root.path().join("argv-pg_dump"),
                "printf '%s\\n' \"$@\" > \"$(dirname \"$4\")/../argv\"\n\
                 printf '%s' \"$PGPASSWORD\" > \"$(dirname \"$4\")/../password\"\n\
                 head -c 10 /dev/zero > \"$4\"\n",
            ),
            ..fakes().clone()
        };
        scene.run("20261026T031500Z", &tools).await.expect("backup");
        let argv = std::fs::read_to_string(scene.root.path().join("argv")).expect("argv");
        assert!(!argv.contains("s3cret"), "{argv}");
        assert_eq!(
            std::fs::read_to_string(scene.root.path().join("password")).expect("pw"),
            "s3cret"
        );
    }
}

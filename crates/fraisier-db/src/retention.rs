//! Which backup archives a retention policy removes.
//!
//! A policy has three independent rules:
//! - an age rule (`retention`): archives older than it are removal candidates;
//! - a floor (`keep_minimum`): the newest that many **valid** archives are exempt
//!   from the age rule;
//! - a ceiling (`keep_last`): only the newest that many survivors are kept,
//!   however young the rest are.
//!
//! The floor is applied *before* the age rule. A producer that has stalled leaves
//! every archive older than the cutoff, and without the floor the whole corpus
//! would age out together.
//!
//! The floor counts valid archives, not files. An archive `pg_restore --list`
//! rejects gives up its slot, so the next valid one takes it; one that cannot be
//! verified (no `pg_restore` on the host) spends a slot as before, so a missing
//! binary does not change what is retained. What this guarantees is exact: no
//! valid archive is removed that a validity-blind floor would have kept, and
//! anything newly removed is both unreadable and already past the cutoff.
//!
//! Verification is bounded to the archives that contest a floor slot. Nothing is
//! verified when the floor is zero, when nothing is past the cutoff, or once the
//! floor is full.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::archive::UtcStamp;

/// A retention policy, valid by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    retention: Option<Duration>,
    keep_minimum: usize,
    keep_last: Option<usize>,
}

/// Why a [`Policy`] was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// `keep_last` is zero, which would remove every archive.
    #[error(
        "keep_last must be at least 1: the newest archive is the restore point a \
         failed migration rolls back to"
    )]
    KeepLastZero,
    /// `keep_last` is below `keep_minimum`.
    #[error(
        "keep_last = {keep_last} is below keep_minimum = {keep_minimum}: a corpus cannot \
         hold at least {keep_minimum} and at most {keep_last} archives. Raise keep_last \
         or lower keep_minimum"
    )]
    KeepLastBelowMinimum {
        /// The ceiling asked for.
        keep_last: usize,
        /// The floor asked for.
        keep_minimum: usize,
    },
}

impl Policy {
    /// A policy with an optional age rule, a floor and an optional ceiling.
    ///
    /// `keep_last < keep_minimum` is rejected rather than resolved: asking for
    /// "at least 3" and "at most 2" is a mistake, and silently honouring either is
    /// how a configuration ends up meaning something nobody chose.
    ///
    /// # Errors
    /// `keep_last` is zero or below `keep_minimum`.
    pub const fn new(
        retention: Option<Duration>,
        keep_minimum: usize,
        keep_last: Option<usize>,
    ) -> Result<Self, PolicyError> {
        if let Some(keep_last) = keep_last {
            if keep_last == 0 {
                return Err(PolicyError::KeepLastZero);
            }
            if keep_last < keep_minimum {
                return Err(PolicyError::KeepLastBelowMinimum {
                    keep_last,
                    keep_minimum,
                });
            }
        }
        Ok(Self {
            retention,
            keep_minimum,
            keep_last,
        })
    }
}

/// One archive a policy is applied to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The archive's path.
    pub path: PathBuf,
    /// When it was taken.
    pub taken_at: SystemTime,
}

/// Whether an archive reads back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// `pg_restore --list` read it.
    Valid,
    /// `pg_restore --list` rejected it, for the reason given.
    Invalid(String),
    /// It could not be checked (for example, no `pg_restore`), for the reason
    /// given.
    Unverifiable(String),
}

/// What a policy does to a set of archives.
///
/// `remove`, `keep` and `exempt` partition the candidates. `invalid`,
/// `unverifiable` and `removed_by_ceiling` are overlays: each archive they name
/// is also in exactly one of the three.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Archives to remove.
    pub remove: Vec<PathBuf>,
    /// Archives kept because they are within the age rule.
    pub keep: Vec<PathBuf>,
    /// Archives past the age rule, kept by the floor.
    pub exempt: Vec<PathBuf>,
    /// Archives `pg_restore --list` rejected while the floor was allocated, with
    /// the reason. Not a corpus audit: only archives that contested a slot were
    /// checked.
    pub invalid: Vec<(PathBuf, String)>,
    /// Archives that could not be checked while the floor was allocated, with
    /// the reason. Each holds the slot it contested, as a valid one would, so
    /// the floor may rest on archives nothing has read.
    pub unverifiable: Vec<(PathBuf, String)>,
    /// Archives the ceiling removed that the age rule would have kept. They were
    /// not old, so a report that calls them "old archives" is wrong about them.
    pub removed_by_ceiling: Vec<PathBuf>,
}

impl Plan {
    /// Every survivor is past the age rule: the floor is the only reason the
    /// corpus is not empty, which means backups have stopped arriving.
    #[must_use]
    pub const fn floor_is_load_bearing(&self) -> bool {
        !self.exempt.is_empty() && self.keep.is_empty()
    }
}

/// Apply `policy` at `now` to `candidates`, calling `verify` only for an archive
/// that contests a floor slot.
pub async fn plan<F, Fut>(
    candidates: Vec<Candidate>,
    policy: &Policy,
    now: SystemTime,
    verify: F,
) -> Plan
where
    F: FnMut(PathBuf) -> Fut,
    Fut: Future<Output = Verdict>,
{
    let mut candidates = candidates;
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.taken_at));
    // No age rule means nothing is past the cutoff, so only the ceiling removes.
    let cutoff = policy
        .retention
        .and_then(|retention| now.checked_sub(retention));
    let mut verify = verify;
    let mut plan = Plan::default();
    // Slots spent, not a position in the list. They coincide when every archive
    // is valid, which is what keeps an unverifiable corpus retaining as before.
    let mut slots = 0;

    for Candidate { path, taken_at } in candidates {
        let beyond_ceiling = policy.keep_last.is_some_and(|keep_last| slots >= keep_last);
        let within_retention = cutoff.is_none_or(|cutoff| taken_at >= cutoff);

        if within_retention && !beyond_ceiling {
            plan.keep.push(path);
            slots += 1;
            continue;
        }
        if !beyond_ceiling && slots < policy.keep_minimum {
            match verify(path.clone()).await {
                // No slot spent, so the next valid archive takes it. Removed only
                // because it is already past the cutoff.
                Verdict::Invalid(reason) => plan.invalid.push((path.clone(), reason)),
                Verdict::Valid => {
                    plan.exempt.push(path);
                    slots += 1;
                    continue;
                }
                Verdict::Unverifiable(reason) => {
                    plan.unverifiable.push((path.clone(), reason));
                    plan.exempt.push(path);
                    slots += 1;
                    continue;
                }
            }
        }
        if within_retention {
            plan.removed_by_ceiling.push(path.clone());
        }
        plan.remove.push(path);
    }
    plan
}

/// The archives in a directory that a retention entry applies to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Corpus {
    /// The archives a policy may act on.
    pub candidates: Vec<Candidate>,
    /// Matching symlinks that resolve outside the directory (or nowhere). They
    /// are never candidates, so nothing ever removes them.
    pub outside: Vec<PathBuf>,
}

/// Whether `name` matches the file-name glob `pattern` (`*` is any run of
/// characters, `?` any one character, everything else is literal).
#[must_use]
pub fn matches(pattern: &str, name: &str) -> bool {
    let (pattern, name) = (pattern.as_bytes(), name.as_bytes());
    let (mut p, mut n) = (0, 0);
    // Where the last `*` was, and how much of `name` it has swallowed so far.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&byte) if byte == b'?' || byte == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((star_p, star_n)) => {
                    p = star_p + 1;
                    n = star_n + 1;
                    star = Some((star_p, star_n + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&byte| byte == b'*')
}

/// The files in `dir` whose names match `pattern`.
///
/// Hidden files (an in-progress `.<name>.partial` among them) and directories
/// are never part of a corpus. An archive is dated by the
/// `-yyyymmddThhmmssZ` stamp before its extension when it has one, and by its
/// mtime otherwise.
///
/// # Errors
/// Reading `dir` fails, including when it does not exist.
pub fn corpus(dir: &Path, pattern: &str) -> std::io::Result<Corpus> {
    let root = dir.canonicalize()?;
    let mut found = Corpus::default();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') || !matches(pattern, &name) {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            match path.canonicalize() {
                Ok(target) if !target.starts_with(&root) => {
                    found.outside.push(path);
                    continue;
                }
                Ok(target) if !target.is_file() => continue,
                Ok(_) => {}
                Err(_) => {
                    found.outside.push(path);
                    continue;
                }
            }
        } else if !file_type.is_file() {
            continue;
        }
        let taken_at = match stamp_in_name(&name) {
            Some(stamp) => stamp.to_system_time(),
            None => path.metadata()?.modified()?,
        };
        found.candidates.push(Candidate { path, taken_at });
    }
    Ok(found)
}

/// The `-yyyymmddThhmmssZ` stamp just before the extension of `name`, if any.
fn stamp_in_name(name: &str) -> Option<UtcStamp> {
    let (stem, _extension) = name.rsplit_once('.')?;
    let (_, stamp) = stem.rsplit_once('-')?;
    UtcStamp::parse(stamp)
}

/// The bytes an unprivileged writer can still use on the filesystem holding
/// `path`.
///
/// # Errors
/// The filesystem could not be queried (for example, `path` does not exist).
pub fn available_bytes(path: &Path) -> std::io::Result<u64> {
    let stats = nix::sys::statvfs::statvfs(path).map_err(std::io::Error::from)?;
    // Reason: both are `u64` on Linux but 32-bit on macOS, a release target.
    #[allow(clippy::useless_conversion)]
    let (blocks, fragment) = (
        u64::from(stats.blocks_available()),
        u64::from(stats.fragment_size()),
    );
    Ok(blocks.saturating_mul(fragment))
}

/// Remove `archives` from `dir`, each only if it is still a file directly in
/// `dir` that resolves inside it. Returns the ones left in place, with why.
#[must_use]
pub fn remove(dir: &Path, archives: &[PathBuf]) -> Vec<(PathBuf, std::io::Error)> {
    let root = match dir.canonicalize() {
        Ok(root) => root,
        Err(error) => {
            return archives
                .iter()
                .map(|archive| {
                    (
                        archive.clone(),
                        std::io::Error::new(error.kind(), error.to_string()),
                    )
                })
                .collect();
        }
    };
    let mut left = Vec::new();
    for archive in archives {
        // `remove_file` on a symlink removes the link, never its target, and the
        // path itself is always directly in `dir`: a link re-pointed after the
        // check can only cost that one link, never a file outside.
        let removed = if archive.parent() == Some(dir) {
            archive.canonicalize().and_then(|target| {
                if target.starts_with(&root) {
                    std::fs::remove_file(archive)
                } else {
                    Err(std::io::Error::other(format!(
                        "it resolves to {}, outside {}",
                        target.display(),
                        dir.display()
                    )))
                }
            })
        } else {
            Err(std::io::Error::other(format!(
                "it is not directly in {}",
                dir.display()
            )))
        };
        if let Err(error) = removed {
            left.push((archive.clone(), error));
        }
    }
    left
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use super::{plan, Candidate, Plan, Policy, PolicyError, Verdict};

    const NOW: Duration = Duration::from_hours(500_000);
    const CUTOFF_HOURS: u64 = 48;

    /// Corpus shapes for the invariant tests: ages in hours against a 48 h cutoff,
    /// and the floor.
    const SHAPES: &[(&[(&str, u64)], usize)] = &[
        (&[("x1", 50), ("x2", 60), ("x3", 70)], 1),
        (&[("y1", 10), ("y2", 60), ("y3", 70)], 2),
        (&[("z1", 50), ("z2", 51), ("z3", 52), ("z4", 53)], 3),
        (&[("w1", 1), ("w2", 2)], 2),
    ];

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + NOW
    }

    fn corpus(ages: &[(&str, u64)]) -> Vec<Candidate> {
        ages.iter()
            .map(|(name, hours)| Candidate {
                path: PathBuf::from(format!("/backups/{name}.pgdump")),
                taken_at: now() - Duration::from_hours(*hours),
            })
            .collect()
    }

    fn path(name: &str) -> PathBuf {
        PathBuf::from(format!("/backups/{name}.pgdump"))
    }

    fn policy(keep_minimum: usize) -> Policy {
        Policy::new(Some(Duration::from_hours(CUTOFF_HOURS)), keep_minimum, None).expect("policy")
    }

    /// Run the planner with `verdicts` by name (default valid), recording every
    /// archive it verified.
    fn run(
        ages: &[(&str, u64)],
        policy: &Policy,
        verdicts: &BTreeMap<String, Verdict>,
    ) -> (Plan, Vec<PathBuf>) {
        let verified = RefCell::new(Vec::new());
        let plan = block_on(plan(corpus(ages), policy, now(), |archive: PathBuf| {
            verified.borrow_mut().push(archive.clone());
            let name = archive
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_owned();
            let verdict = verdicts.get(&name).cloned().unwrap_or(Verdict::Valid);
            std::future::ready(verdict)
        }));
        (plan, verified.into_inner())
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    fn invalid(names: &[&str]) -> BTreeMap<String, Verdict> {
        names
            .iter()
            .map(|name| ((*name).to_owned(), Verdict::Invalid("stubbed".to_owned())))
            .collect()
    }

    fn unverifiable(ages: &[(&str, u64)]) -> BTreeMap<String, Verdict> {
        ages.iter()
            .map(|(name, _)| {
                (
                    (*name).to_owned(),
                    Verdict::Unverifiable("no pg_restore".to_owned()),
                )
            })
            .collect()
    }

    fn invalid_paths(plan: &Plan) -> BTreeSet<PathBuf> {
        plan.invalid.iter().map(|(path, _)| path.clone()).collect()
    }

    #[test]
    fn the_slot_goes_to_the_valid_archive_not_the_newest() {
        let (plan, _) = run(
            &[("newest_truncated", 50), ("older_good", 60)],
            &policy(1),
            &invalid(&["newest_truncated"]),
        );
        assert_eq!(plan.exempt, [path("older_good")]);
        assert_eq!(plan.remove, [path("newest_truncated")]);
    }

    #[test]
    fn an_invalid_archive_within_retention_is_still_kept() {
        let (plan, _) = run(
            &[("fresh_truncated", 1)],
            &policy(1),
            &invalid(&["fresh_truncated"]),
        );
        assert_eq!(plan.keep, [path("fresh_truncated")]);
        assert!(plan.remove.is_empty(), "{:?}", plan.remove);
    }

    #[test]
    fn a_stalled_producer_keeps_real_archives_instead_of_the_corrupt_one() {
        let (plan, _) = run(
            &[("a_truncated", 50), ("b_good", 60), ("c_good", 70)],
            &policy(2),
            &invalid(&["a_truncated"]),
        );
        assert_eq!(plan.exempt, [path("b_good"), path("c_good")]);
        assert!(plan.floor_is_load_bearing());
    }

    #[test]
    fn all_valid_retains_exactly_as_all_unverifiable() {
        for (ages, floor) in SHAPES {
            let (valid, _) = run(ages, &policy(*floor), &BTreeMap::new());
            let (mut blind, _) = run(ages, &policy(*floor), &unverifiable(ages));
            blind.unverifiable.clear();
            assert_eq!(valid, blind, "{ages:?}, floor {floor}");
        }
    }

    #[test]
    fn no_valid_archive_is_newly_removed() {
        for (ages, floor) in SHAPES {
            let (blind, _) = run(ages, &policy(*floor), &BTreeMap::new());
            for (bad, _) in *ages {
                let (plan, _) = run(ages, &policy(*floor), &invalid(&[bad]));
                let bad_paths = invalid_paths(&plan);
                for removed in &plan.remove {
                    assert!(
                        bad_paths.contains(removed) || blind.remove.contains(removed),
                        "{removed:?} was valid and newly removed ({ages:?}, floor {floor}, bad {bad})"
                    );
                }
            }
        }
    }

    #[test]
    fn anything_newly_removed_is_invalid_and_past_the_cutoff() {
        for (ages, floor) in SHAPES {
            let (blind, _) = run(ages, &policy(*floor), &BTreeMap::new());
            for (bad, _) in *ages {
                let (plan, _) = run(ages, &policy(*floor), &invalid(&[bad]));
                let bad_paths = invalid_paths(&plan);
                for removed in plan.remove.iter().filter(|p| !blind.remove.contains(p)) {
                    assert!(bad_paths.contains(removed), "{removed:?} was not invalid");
                    let age = ages
                        .iter()
                        .find(|(name, _)| path(name) == *removed)
                        .map(|(_, hours)| *hours)
                        .expect("in corpus");
                    assert!(age > CUTOFF_HOURS, "{removed:?} was within retention");
                }
            }
        }
    }

    #[test]
    fn without_pg_restore_retention_is_unchanged() {
        let ages: &[(&str, u64)] = &[("a", 50), ("b", 60), ("c", 70)];
        let (baseline, _) = run(ages, &policy(2), &BTreeMap::new());
        let (plan, _) = run(ages, &policy(2), &unverifiable(ages));
        assert_eq!(plan.exempt, baseline.exempt);
        assert_eq!(plan.remove, baseline.remove);
        assert!(plan.invalid.is_empty(), "{:?}", plan.invalid);
    }

    #[test]
    fn an_unverifiable_archive_holding_a_floor_slot_is_named() {
        let ages: &[(&str, u64)] = &[("a", 50), ("b", 60), ("c", 70)];
        let (plan, _) = run(ages, &policy(2), &unverifiable(&ages[..1]));
        assert_eq!(plan.exempt, [path("a"), path("b")]);
        assert_eq!(plan.unverifiable, [(path("a"), "no pg_restore".to_owned())]);
    }

    #[test]
    fn an_invalid_archive_lands_in_exactly_one_group() {
        let (plan, _) = run(
            &[("bad_old", 50), ("bad_older", 55), ("good", 60)],
            &policy(1),
            &invalid(&["bad_old", "bad_older"]),
        );
        assert_eq!(plan.invalid.len(), 2);
        for (name, _) in &plan.invalid {
            let landed = [&plan.remove, &plan.keep, &plan.exempt]
                .iter()
                .filter(|group| group.contains(name))
                .count();
            assert_eq!(landed, 1, "{name:?}");
        }
    }

    #[test]
    fn the_partition_covers_every_candidate() {
        let (plan, _) = run(
            &[("d0", 1), ("d1", 50), ("d2", 60), ("d3", 70)],
            &policy(1),
            &invalid(&["d1"]),
        );
        assert_eq!(plan.remove.len() + plan.keep.len() + plan.exempt.len(), 4);
    }

    #[test]
    fn a_fresh_archive_is_not_verified() {
        let (plan, verified) = run(&[("fresh_bad", 1)], &policy(2), &invalid(&["fresh_bad"]));
        assert!(verified.is_empty(), "{verified:?}");
        assert!(plan.invalid.is_empty(), "{:?}", plan.invalid);
    }

    #[test]
    fn archives_past_a_full_floor_are_not_verified() {
        let (_, verified) = run(
            &[("a", 50), ("b", 60), ("c", 70)],
            &policy(1),
            &BTreeMap::new(),
        );
        assert_eq!(verified, [path("a")]);
    }

    #[test]
    fn a_zero_floor_verifies_nothing() {
        let (plan, verified) = run(&[("a", 50), ("b", 60)], &policy(0), &BTreeMap::new());
        assert!(verified.is_empty(), "{verified:?}");
        assert_eq!(plan.remove, [path("a"), path("b")]);
    }

    #[test]
    fn nothing_past_the_cutoff_verifies_nothing() {
        let (_, verified) = run(&[("a", 1), ("b", 2)], &policy(3), &BTreeMap::new());
        assert!(verified.is_empty(), "{verified:?}");
    }

    #[test]
    fn a_third_archive_past_the_age_rule_removes_the_oldest() {
        let one_day = Policy::new(Some(Duration::from_hours(24)), 2, None).expect("policy");
        let (plan, _) = run(
            &[("new", 30), ("mid", 40), ("old", 50)],
            &one_day,
            &BTreeMap::new(),
        );
        assert_eq!(plan.remove, [path("old")]);
        assert_eq!(plan.exempt, [path("new"), path("mid")]);
    }

    #[test]
    fn nothing_below_the_floor_is_removed_when_every_archive_is_past_the_age_rule() {
        let one_day = Policy::new(Some(Duration::from_hours(24)), 2, None).expect("policy");
        let (plan, _) = run(&[("a", 100), ("b", 200)], &one_day, &BTreeMap::new());
        assert!(plan.remove.is_empty(), "{:?}", plan.remove);
        assert!(plan.floor_is_load_bearing());
    }

    #[test]
    fn candidates_are_ordered_by_when_they_were_taken_not_by_input_order() {
        let (plan, _) = run(&[("old", 70), ("new", 50)], &policy(1), &BTreeMap::new());
        assert_eq!(plan.exempt, [path("new")]);
        assert_eq!(plan.remove, [path("old")]);
    }

    #[test]
    fn the_ceiling_removes_young_archives_and_says_so() {
        let capped =
            Policy::new(Some(Duration::from_hours(CUTOFF_HOURS)), 1, Some(2)).expect("policy");
        let (plan, _) = run(
            &[("a", 1), ("b", 2), ("c", 3), ("d", 60)],
            &capped,
            &BTreeMap::new(),
        );
        assert_eq!(plan.keep, [path("a"), path("b")]);
        assert_eq!(plan.remove, [path("c"), path("d")]);
        assert_eq!(plan.removed_by_ceiling, [path("c")]);
    }

    #[test]
    fn a_ceiling_alone_bounds_the_corpus_by_count() {
        let capped = Policy::new(None, 0, Some(1)).expect("policy");
        let (plan, _) = run(&[("a", 1), ("b", 1_000)], &capped, &BTreeMap::new());
        assert_eq!(plan.keep, [path("a")]);
        assert_eq!(plan.removed_by_ceiling, [path("b")]);
    }

    #[test]
    fn no_rule_removes_nothing() {
        let none = Policy::new(None, 3, None).expect("policy");
        let (plan, _) = run(&[("a", 1), ("b", 1_000)], &none, &BTreeMap::new());
        assert!(plan.remove.is_empty(), "{:?}", plan.remove);
    }

    #[test]
    fn a_ceiling_below_the_floor_is_rejected() {
        assert_eq!(
            Policy::new(None, 3, Some(2)),
            Err(PolicyError::KeepLastBelowMinimum {
                keep_last: 2,
                keep_minimum: 3
            })
        );
    }

    #[test]
    fn a_zero_ceiling_is_rejected() {
        assert_eq!(
            Policy::new(None, 0, Some(0)),
            Err(PolicyError::KeepLastZero)
        );
    }

    mod files {
        use std::path::{Path, PathBuf};
        use std::time::{Duration, SystemTime};

        use super::super::{available_bytes, corpus, matches, remove, Candidate};

        #[test]
        fn a_real_directory_has_some_space_available() {
            let dir = tempfile::tempdir().expect("tempdir");
            assert!(available_bytes(dir.path()).expect("statvfs") > 0);
        }

        #[test]
        fn a_missing_path_has_no_measurable_space() {
            let dir = tempfile::tempdir().expect("tempdir");
            assert!(available_bytes(&dir.path().join("absent")).is_err());
        }

        fn touch(dir: &Path, name: &str) -> PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, b"PGDMP").expect("write");
            path
        }

        fn names(candidates: &[Candidate]) -> Vec<String> {
            let mut names: Vec<String> = candidates
                .iter()
                .map(|c| {
                    c.path
                        .file_name()
                        .expect("name")
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            names.sort();
            names
        }

        #[test]
        fn the_glob_matches_stars_and_question_marks_literally_otherwise() {
            assert!(matches(
                "*.pgdump",
                "shop-production-20261026T030000Z.pgdump"
            ));
            assert!(matches(
                "shop-*-2026????T*Z.pgdump",
                "shop-production-20261026T030000Z.pgdump"
            ));
            assert!(matches("*", "anything"));
            assert!(matches("a*b*c", "abc"));
            assert!(!matches("*.pgdump", "shop.pgdump.partial"));
            assert!(!matches("*.pgdump", "shop.dump"));
            assert!(!matches("?.pgdump", "ab.pgdump"));
            assert!(!matches("a*b", "acb-"));
        }

        #[test]
        fn a_corpus_is_the_matching_plain_files() {
            let dir = tempfile::tempdir().expect("tempdir");
            touch(dir.path(), "a.pgdump");
            touch(dir.path(), "b.pgdump");
            touch(dir.path(), "notes.txt");
            touch(dir.path(), ".c.pgdump");
            std::fs::create_dir(dir.path().join("d.pgdump")).expect("mkdir");
            let found = corpus(dir.path(), "*.pgdump").expect("corpus");
            assert_eq!(names(&found.candidates), ["a.pgdump", "b.pgdump"]);
            assert!(found.outside.is_empty(), "{:?}", found.outside);
        }

        #[test]
        fn an_in_progress_partial_is_never_part_of_a_corpus() {
            let dir = tempfile::tempdir().expect("tempdir");
            touch(
                dir.path(),
                ".shop-production-20261026T030000Z.pgdump.partial",
            );
            let found = corpus(dir.path(), "*").expect("corpus");
            assert!(found.candidates.is_empty(), "{:?}", found.candidates);
        }

        #[test]
        fn the_stamp_in_the_name_dates_an_archive_over_its_mtime() {
            let dir = tempfile::tempdir().expect("tempdir");
            touch(dir.path(), "shop-production-20261026T030000Z.pgdump");
            let found = corpus(dir.path(), "*.pgdump").expect("corpus");
            // 2026-10-26T03:00:00Z.
            assert_eq!(
                found.candidates[0].taken_at,
                SystemTime::UNIX_EPOCH + Duration::from_hours(498_051)
            );
        }

        #[test]
        fn an_archive_without_a_stamp_is_dated_by_its_mtime() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = touch(dir.path(), "legacy.pgdump");
            let then = SystemTime::UNIX_EPOCH + Duration::from_hours(400_000);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(then))
                .expect("set mtime");
            let found = corpus(dir.path(), "*.pgdump").expect("corpus");
            assert_eq!(found.candidates[0].taken_at, then);
        }

        #[test]
        fn a_symlink_inside_the_dir_is_a_candidate() {
            let dir = tempfile::tempdir().expect("tempdir");
            touch(dir.path(), "real.pgdump");
            std::os::unix::fs::symlink(
                dir.path().join("real.pgdump"),
                dir.path().join("link.pgdump"),
            )
            .expect("symlink");
            let found = corpus(dir.path(), "*.pgdump").expect("corpus");
            assert_eq!(names(&found.candidates), ["link.pgdump", "real.pgdump"]);
        }

        #[test]
        fn a_symlink_out_of_the_dir_or_nowhere_is_set_aside() {
            let outside = tempfile::tempdir().expect("tempdir");
            let target = touch(outside.path(), "precious.pgdump");
            let dir = tempfile::tempdir().expect("tempdir");
            std::os::unix::fs::symlink(&target, dir.path().join("escape.pgdump")).expect("symlink");
            std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("dangling.pgdump"))
                .expect("symlink");
            let found = corpus(dir.path(), "*.pgdump").expect("corpus");
            assert!(found.candidates.is_empty(), "{:?}", found.candidates);
            let mut outside_names: Vec<_> = found
                .outside
                .iter()
                .map(|p| p.file_name().expect("name").to_string_lossy().into_owned())
                .collect();
            outside_names.sort();
            assert_eq!(outside_names, ["dangling.pgdump", "escape.pgdump"]);
        }

        #[test]
        fn a_missing_dir_is_an_error() {
            let dir = tempfile::tempdir().expect("tempdir");
            let error = corpus(&dir.path().join("absent"), "*.pgdump").expect_err("missing");
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        }

        #[test]
        fn remove_deletes_exactly_the_archives_given() {
            let dir = tempfile::tempdir().expect("tempdir");
            let gone = touch(dir.path(), "a.pgdump");
            let stays = touch(dir.path(), "b.pgdump");
            let left = remove(dir.path(), std::slice::from_ref(&gone));
            assert!(left.is_empty(), "{left:?}");
            assert!(!gone.exists());
            assert!(stays.exists());
        }

        #[test]
        fn remove_leaves_a_path_outside_the_dir_in_place() {
            let outside = tempfile::tempdir().expect("tempdir");
            let precious = touch(outside.path(), "precious.pgdump");
            let dir = tempfile::tempdir().expect("tempdir");
            let left = remove(dir.path(), std::slice::from_ref(&precious));
            assert_eq!(left.len(), 1, "{left:?}");
            assert!(precious.exists());
        }

        #[test]
        fn remove_leaves_a_link_repointed_outside_since_listing_and_its_target() {
            let outside = tempfile::tempdir().expect("tempdir");
            let precious = touch(outside.path(), "precious.pgdump");
            let dir = tempfile::tempdir().expect("tempdir");
            let link = dir.path().join("link.pgdump");
            std::os::unix::fs::symlink(&precious, &link).expect("symlink");
            let left = remove(dir.path(), std::slice::from_ref(&link));
            assert_eq!(left.len(), 1, "{left:?}");
            assert!(precious.exists());
            assert!(link.symlink_metadata().is_ok());
        }

        #[test]
        fn remove_unlinks_a_link_inside_the_dir_but_not_its_target() {
            let dir = tempfile::tempdir().expect("tempdir");
            let real = touch(dir.path(), "real.pgdump");
            let link = dir.path().join("link.pgdump");
            std::os::unix::fs::symlink(&real, &link).expect("symlink");
            let left = remove(dir.path(), std::slice::from_ref(&link));
            assert!(left.is_empty(), "{left:?}");
            assert!(link.symlink_metadata().is_err());
            assert!(real.exists());
        }
    }
}

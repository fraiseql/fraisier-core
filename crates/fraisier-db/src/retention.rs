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
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

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
/// `remove`, `keep` and `exempt` partition the candidates. `invalid` and
/// `removed_by_ceiling` are overlays: each archive they name is also in exactly
/// one of the three.
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
                Verdict::Valid | Verdict::Unverifiable(_) => {
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
            let (blind, _) = run(ages, &policy(*floor), &unverifiable(ages));
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
}

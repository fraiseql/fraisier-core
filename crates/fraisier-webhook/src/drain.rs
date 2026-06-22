//! Self-upgrade drain coordination (Python fraisier 0.31).
//!
//! Before a coordinated webhook restart, the upgrade path raises a drain flag (so
//! the running server starts answering new deploys with `503` — see
//! [`crate::server::Drain`]), waits a settle window for dispatch-accepted deploys
//! to reach their lock, then polls until every in-flight deploy lock has cleared
//! or a timeout elapses. Only then is the restart safe.
//!
//! The "in-flight" signal is the per-fraise [`StateStore`] lock (PRD §9.4): a
//! deploy holds it from start to commit/rollback. The probe is injected so this
//! coordinator is pure logic — the CLI supplies a real probe that lists the held
//! `*.lock` files for the filesystem backend (the `lock_backend=database` caveat
//! from Python 0.31 applies: that backend sees no lock files and drains
//! immediately, no worse than before).
//!
//! [`StateStore`]: https://docs.rs/fraisier-saga

use std::path::Path;
use std::time::{Duration, Instant};

/// Drain timing, from the `[webhook].self_upgrade_*` config keys.
#[derive(Debug, Clone, Copy)]
pub struct DrainTuning {
    /// How long to wait for in-flight deploys before giving up.
    pub timeout: Duration,
    /// How often to re-check for in-flight deploys.
    pub poll: Duration,
    /// Settle window after raising the flag, before the first check.
    pub settle: Duration,
}

impl Default for DrainTuning {
    fn default() -> Self {
        Self {
            timeout: Duration::from_mins(10),
            poll: Duration::from_secs(1),
            settle: Duration::from_secs(2),
        }
    }
}

/// How a drain ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// In-flight deploys cleared; the restart is safe to issue. The drain flag is
    /// left raised for the caller (the restart clears it by starting fresh).
    Drained,
    /// The timeout elapsed with these deploy locks still held. The flag has been
    /// lowered (the server resumes serving) and the unit is left **unrestarted**
    /// for operator intervention.
    TimedOut {
        /// The lock names still held at timeout.
        held: Vec<String>,
    },
}

/// Coordinate a self-upgrade drain over `flag_path`.
///
/// Raises the flag, waits `tuning.settle`, then polls `in_flight` every
/// `tuning.poll` until it reports no held locks ([`DrainOutcome::Drained`]) or
/// `tuning.timeout` elapses ([`DrainOutcome::TimedOut`], flag lowered).
///
/// # Errors
/// [`std::io::Error`] if the flag file cannot be created.
pub async fn drain_in_flight<F>(
    flag_path: &Path,
    tuning: DrainTuning,
    mut in_flight: F,
) -> std::io::Result<DrainOutcome>
where
    F: FnMut() -> Vec<String>,
{
    // Raise the flag first: the running server begins refusing new deploys before
    // we look at what is already in flight.
    std::fs::write(flag_path, b"draining\n")?;
    tokio::time::sleep(tuning.settle).await;

    let start = Instant::now();
    loop {
        let held = in_flight();
        if held.is_empty() {
            return Ok(DrainOutcome::Drained);
        }
        if start.elapsed() >= tuning.timeout {
            // Lower the flag so the server resumes serving; the unit is not
            // restarted (the caller surfaces this with a distinct exit code).
            let _ = std::fs::remove_file(flag_path);
            return Ok(DrainOutcome::TimedOut { held });
        }
        tokio::time::sleep(tuning.poll).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{drain_in_flight, DrainOutcome, DrainTuning};
    use std::cell::Cell;
    use std::time::Duration;

    fn fast() -> DrainTuning {
        DrainTuning {
            timeout: Duration::from_secs(5),
            poll: Duration::ZERO,
            settle: Duration::ZERO,
        }
    }

    fn flag_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "fraisier-drain-test-{}-{tag}.flag",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn drains_once_in_flight_clears_and_leaves_the_flag_up() {
        let flag = flag_path("clears");
        let _ = std::fs::remove_file(&flag);
        // Held on the first poll, clear on the second.
        let calls = Cell::new(0u32);
        let outcome = drain_in_flight(&flag, fast(), || {
            let n = calls.get();
            calls.set(n + 1);
            if n == 0 {
                vec!["checkout/production".to_owned()]
            } else {
                Vec::new()
            }
        })
        .await
        .expect("drain");
        assert_eq!(outcome, DrainOutcome::Drained);
        assert!(flag.exists(), "the flag stays up; the restart clears it");
        let _ = std::fs::remove_file(&flag);
    }

    #[tokio::test]
    async fn times_out_with_held_locks_and_lowers_the_flag() {
        let flag = flag_path("timeout");
        let _ = std::fs::remove_file(&flag);
        let tuning = DrainTuning {
            timeout: Duration::ZERO, // the first still-held check trips the timeout
            poll: Duration::ZERO,
            settle: Duration::ZERO,
        };
        let outcome = drain_in_flight(&flag, tuning, || vec!["checkout/production".to_owned()])
            .await
            .expect("drain");
        assert_eq!(
            outcome,
            DrainOutcome::TimedOut {
                held: vec!["checkout/production".to_owned()]
            }
        );
        assert!(
            !flag.exists(),
            "a timeout lowers the flag so the server resumes serving"
        );
    }
}

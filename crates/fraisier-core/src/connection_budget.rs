//! The blue-green **connection-budget** edge.
//!
//! During the hold window **both fleets are live**, so connections into the
//! shared Postgres transiently double; exhausting `max_connections` mid-cutover is
//! a real outage. fraisier addresses it **without** building a connection-router
//! (held out of scope): a documented operator prerequisite (steady-state headroom
//! ≥ the second fleet's pool) **plus** a cheap pre-swap pre-flight check.
//!
//! The check reads the DB — `SHOW max_connections` + `SELECT count(*) FROM
//! pg_stat_activity` over the existing migration DSN (PG* env, never argv) — and
//! applies [`evaluate`]: if `current + green_pool` would exceed `max_connections`
//! it **hard-refuses before the swap**; within a configurable margin it **warns**.
//! It does not manage or pool connections — that stays the held-out router.

use async_trait::async_trait;

use crate::adapter_axes::AdapterCtx;

/// A snapshot of the shared Postgres' connection state, from the pre-swap probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSnapshot {
    /// `SHOW max_connections`.
    pub max_connections: u32,
    /// `SELECT count(*) FROM pg_stat_activity` — connections in use right now.
    pub current: u32,
}

/// The connection-budget verdict for the swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetVerdict {
    /// Enough headroom for both fleets — proceed.
    Ok,
    /// Within the margin — proceed but warn.
    Warn(String),
    /// Doubling connections would exceed `max_connections` — refuse before swap.
    Refuse(String),
}

/// A pre-swap connection-budget probe (reads the shared DB).
#[async_trait]
pub trait ConnectionBudget: Send + Sync {
    /// Probe the shared Postgres for its connection state.
    ///
    /// # Errors
    /// A human-readable message if the DB cannot be probed.
    async fn probe(&self, ctx: &AdapterCtx) -> Result<BudgetSnapshot, String>;
}

/// Decide whether the green fleet's pool fits in the remaining headroom.
///
/// `green_pool` is the second fleet's connection pool size; `margin` is the
/// warn-band below `max_connections`. Refuse if `current + green_pool >
/// max_connections`; warn if it lands within `margin` of the ceiling; else Ok.
#[must_use]
pub fn evaluate(snapshot: BudgetSnapshot, green_pool: u32, margin: u32) -> BudgetVerdict {
    let projected = snapshot.current.saturating_add(green_pool);
    let max = snapshot.max_connections;
    if projected > max {
        return BudgetVerdict::Refuse(format!(
            "blue-green would need {projected} connections (current {} + green pool {green_pool}) \
             but max_connections is {max}: refusing before the swap (raise max_connections or \
             shrink the pool)",
            snapshot.current
        ));
    }
    if projected > max.saturating_sub(margin) {
        return BudgetVerdict::Warn(format!(
            "blue-green will use {projected}/{max} connections — within the {margin}-connection \
             safety margin"
        ));
    }
    BudgetVerdict::Ok
}

#[cfg(test)]
mod tests {
    use super::{evaluate, BudgetSnapshot, BudgetVerdict};

    fn snap(max: u32, current: u32) -> BudgetSnapshot {
        BudgetSnapshot {
            max_connections: max,
            current,
        }
    }

    #[test]
    fn refuses_when_doubling_would_exceed_max_connections() {
        // 90 in use + a 20-connection green pool = 110 > 100.
        let verdict = evaluate(snap(100, 90), 20, 10);
        assert!(matches!(verdict, BudgetVerdict::Refuse(_)), "{verdict:?}");
    }

    #[test]
    fn warns_within_the_margin() {
        // 75 + 20 = 95, which is within 10 of 100 (i.e. > 90).
        let verdict = evaluate(snap(100, 75), 20, 10);
        assert!(matches!(verdict, BudgetVerdict::Warn(_)), "{verdict:?}");
    }

    #[test]
    fn ok_with_comfortable_headroom() {
        // 40 + 20 = 60, well under 100 - 10.
        assert_eq!(evaluate(snap(100, 40), 20, 10), BudgetVerdict::Ok);
    }

    #[test]
    fn exact_fit_at_the_ceiling_is_not_a_refusal() {
        // 80 + 20 = 100 == max → not a refusal (but within margin → warn).
        let verdict = evaluate(snap(100, 80), 20, 10);
        assert!(matches!(verdict, BudgetVerdict::Warn(_)), "{verdict:?}");
    }
}

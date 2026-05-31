//! Multi-host deploy plan (PRD §5.3).
//!
//! # Schema only
//!
//! Phase 1 defines the *shape* — [`HostInventory`], [`RolloutStrategy`],
//! [`MultiHostPlan`] — so the rest of the foundation can refer to it without a
//! breaking change later. [`MultiHostPlan::execute`] is a stub that returns
//! [`MultiHostError::NotImplemented`]; Phase 4 populates it by composing the
//! saga (no separate state machine). Two shape decisions are deliberately
//! future-proofed (PRD review §7): [`HostEntry`] reserves per-host adapter
//! overrides, and [`RolloutStrategy`] is `#[non_exhaustive]`.

use std::collections::BTreeMap;

use crate::adapter_axes::HostId;

/// One host in a multi-host deploy's inventory.
///
/// `overrides` reserves per-host adapter-axis configuration (a canary host on a
/// different artifact source, an LB segment with different drain semantics, …).
/// Phase 1 only reserves the field; Phase 4 interprets it.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::HostId;
/// # use fraisier_core::multi_host::HostEntry;
/// let entry = HostEntry::new(HostId::new("web-1"), "web1.internal");
/// assert!(entry.overrides.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostEntry {
    /// The host's inventory name.
    pub host: HostId,
    /// The address fraisier reaches it at (hostname or IP).
    pub address: String,
    /// Per-host partial adapter config, merged over the deploy-wide config in
    /// Phase 4. Keyed by axis name (`"artifact"`, `"lb"`, …).
    #[serde(default)]
    pub overrides: BTreeMap<String, serde_json::Value>,
}

impl HostEntry {
    /// Create an inventory entry with no overrides.
    #[must_use]
    pub fn new(host: HostId, address: impl Into<String>) -> Self {
        Self {
            host,
            address: address.into(),
            overrides: BTreeMap::new(),
        }
    }
}

/// The ordered set of hosts a deploy targets.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::HostId;
/// # use fraisier_core::multi_host::{HostEntry, HostInventory};
/// let inv = HostInventory::new()
///     .with_host(HostEntry::new(HostId::new("web-1"), "web1.internal"));
/// assert_eq!(inv.hosts().len(), 1);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostInventory {
    hosts: Vec<HostEntry>,
}

impl HostInventory {
    /// An empty inventory.
    #[must_use]
    pub const fn new() -> Self {
        Self { hosts: Vec::new() }
    }

    /// Append a host (builder style).
    #[must_use]
    pub fn with_host(mut self, host: HostEntry) -> Self {
        self.hosts.push(host);
        self
    }

    /// The hosts, in rollout order.
    #[must_use]
    pub fn hosts(&self) -> &[HostEntry] {
        &self.hosts
    }

    /// Whether the inventory is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

/// How hosts are advanced through a rollout (PRD §5.5).
///
/// `#[non_exhaustive]`: `BlueGreen` (v1.0.0 GA) and `Canary` (later) will be
/// added without it being a breaking change.
///
/// # Example
/// ```
/// # use fraisier_core::multi_host::RolloutStrategy;
/// assert!(matches!(RolloutStrategy::Rolling(2), RolloutStrategy::Rolling(2)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RolloutStrategy {
    /// Every host updated in parallel; brief full downtime tolerated.
    AllAtOnce,
    /// Process this many hosts at a time; the rest stay live.
    Rolling(usize),
}

/// Errors from a multi-host plan.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MultiHostError {
    /// Multi-host execution is reserved for Phase 4.
    #[error("multi-host execution is not implemented until Phase 4")]
    NotImplemented,
}

/// A multi-host deploy plan: an inventory plus a rollout strategy.
///
/// Phase 4 will [`execute`](MultiHostPlan::execute) it by composing the saga
/// engine over the inventory in strategy order. Phase 1 only constructs and
/// inspects it.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::HostId;
/// # use fraisier_core::multi_host::{HostEntry, HostInventory, MultiHostPlan, RolloutStrategy};
/// let inv = HostInventory::new().with_host(HostEntry::new(HostId::new("web-1"), "web1.internal"));
/// let plan = MultiHostPlan::new(inv, RolloutStrategy::Rolling(1));
/// assert_eq!(plan.inventory().hosts().len(), 1);
/// ```
#[derive(Debug, Clone)]
pub struct MultiHostPlan {
    inventory: HostInventory,
    strategy: RolloutStrategy,
}

impl MultiHostPlan {
    /// Build a plan from an inventory and a strategy.
    #[must_use]
    pub const fn new(inventory: HostInventory, strategy: RolloutStrategy) -> Self {
        Self {
            inventory,
            strategy,
        }
    }

    /// The host inventory.
    #[must_use]
    pub const fn inventory(&self) -> &HostInventory {
        &self.inventory
    }

    /// The rollout strategy.
    #[must_use]
    pub const fn strategy(&self) -> RolloutStrategy {
        self.strategy
    }

    /// Execute the plan. **Phase 4** populates this by composing the saga over
    /// the inventory in strategy order; Phase 1 is a stub.
    ///
    /// # Errors
    /// Always returns [`MultiHostError::NotImplemented`] in Phase 1.
    // Reason: the signature is async now to match the Phase 4 implementation, so
    // callers/tests don't change when the logic lands; the stub just doesn't await.
    #[allow(clippy::unused_async)]
    pub async fn execute(&self) -> Result<(), MultiHostError> {
        Err(MultiHostError::NotImplemented)
    }
}

#[cfg(test)]
mod tests {
    use super::{HostEntry, HostInventory, MultiHostError, MultiHostPlan, RolloutStrategy};
    use crate::adapter_axes::HostId;

    fn inventory() -> HostInventory {
        HostInventory::new()
            .with_host(HostEntry::new(HostId::new("web-1"), "web1.internal"))
            .with_host(HostEntry::new(HostId::new("web-2"), "web2.internal"))
            .with_host(HostEntry::new(HostId::new("web-3"), "web3.internal"))
    }

    #[test]
    fn plan_constructs_with_inventory_and_strategy() {
        let plan = MultiHostPlan::new(inventory(), RolloutStrategy::Rolling(1));
        assert_eq!(plan.inventory().hosts().len(), 3);
        assert!(matches!(plan.strategy(), RolloutStrategy::Rolling(1)));
    }

    #[tokio::test]
    async fn execute_is_not_implemented_until_phase_4() {
        let plan = MultiHostPlan::new(inventory(), RolloutStrategy::AllAtOnce);
        assert!(matches!(
            plan.execute().await,
            Err(MultiHostError::NotImplemented)
        ));
    }

    #[test]
    fn host_entry_reserves_per_host_adapter_overrides() {
        let mut entry = HostEntry::new(HostId::new("canary"), "canary.internal");
        entry.overrides.insert(
            "artifact".to_owned(),
            serde_json::json!({ "source": "local" }),
        );
        assert!(entry.overrides.contains_key("artifact"));
    }

    #[test]
    fn strategy_round_trips_through_serde() {
        let strategy = RolloutStrategy::Rolling(2);
        let json = serde_json::to_string(&strategy).expect("serialize");
        let back: RolloutStrategy = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(back, RolloutStrategy::Rolling(2)));
    }
}

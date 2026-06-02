//! The `fraisier.toml` data model (PRD §7.1) and the accessors that resolve it
//! into the frozen `fraisier-core` vocabulary types.
//!
//! Every section is optional at the type level so that the [`SpecQL`
//! preset](crate::SpecqlPreset) can fill them in and so single-host configs can
//! legitimately omit `[hosts]` / `[lb]`. The [validation
//! pass](crate::DeployConfig::validate) is what enforces which sections and
//! fields are actually required.

use std::collections::BTreeMap;
use std::path::PathBuf;

use fraisier_core::adapter_axes::{AdapterCtx, HostId};
use fraisier_core::multi_host::{HostEntry, HostInventory, RolloutStrategy};
use serde::{Deserialize, Serialize};

use crate::SpecqlPreset;

/// The logical secret name the migration DSN is exposed under (Decision 5). The
/// migration adapter resolves it with `AdapterCtx::secret("DATABASE_URL")`.
const DATABASE_URL_LOGICAL: &str = "DATABASE_URL";

/// The default HTTP status a health probe expects when none is configured.
pub const DEFAULT_EXPECTED_STATUS: u16 = 200;

/// The strategy string selecting an all-at-once rollout (either spelling).
pub const STRATEGY_ALL_AT_ONCE: [&str; 2] = ["all-at-once", "all_at_once"];

/// The strategy string selecting a rolling rollout.
pub const STRATEGY_ROLLING: &str = "rolling";

/// A parsed `fraisier.toml`.
///
/// Build one with [`DeployConfig::from_toml_str`] (parse + preset expansion) or
/// [`DeployConfig::load`] (parse + expansion + validation). The accessors
/// ([`rollout_strategy`](DeployConfig::rollout_strategy),
/// [`host_inventory`](DeployConfig::host_inventory),
/// [`migration_adapter_ctx`](DeployConfig::migration_adapter_ctx)) resolve the
/// raw fields into `fraisier-core` types and assume the config has already
/// validated clean.
///
/// # Example
/// ```
/// use fraisier_config::DeployConfig;
///
/// let cfg = DeployConfig::from_toml_str(
///     "[deploy]\nname = \"app\"\nenvironment = \"production\"\n",
/// )
/// .expect("parses");
/// assert_eq!(cfg.deploy.unwrap().name.as_deref(), Some("app"));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployConfig {
    /// The `[deploy]` section: the deployable's identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<DeploySection>,
    /// The `[hosts]` section: inventory + rollout strategy. Absent for
    /// single-host deploys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<HostsSection>,
    /// The `[artifact]` section: where the code/binary comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactSection>,
    /// The `[migration]` section: the migration adapter and its DSN source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<MigrationSection>,
    /// The `[service]` section: the service manager.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceSection>,
    /// The `[health]` section: the health probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthSection>,
    /// The `[lb]` section: the load balancer. Absent when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lb: Option<LbSection>,
    /// The `[specql]` preset. Present only in the *unexpanded* form; both
    /// [`from_toml_str`](DeployConfig::from_toml_str) and
    /// [`load`](DeployConfig::load) consume it and leave this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specql: Option<SpecqlPreset>,
}

/// The `[deploy]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploySection {
    /// The deployable (fraise) name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The target environment (e.g. `"production"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

/// The `[hosts]` section. Parsed in Phase 1; *executed* in Phase 4.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostsSection {
    /// The rollout strategy: `"rolling"` or `"all-at-once"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    /// How many hosts a `"rolling"` strategy advances at a time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling_batch_size: Option<usize>,
    /// The ordered host inventory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inventory: Vec<HostSpec>,
}

/// One `inventory = [{ name, address }]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSpec {
    /// The host's inventory name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The address fraisier reaches it at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// The `[artifact]` section. `source` selects which of the source-specific
/// fields apply (`release` / `git` / `local`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSection {
    /// The artifact source: `"release"`, `"git"`, or `"local"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// `release`: the download URL (may contain a `{version}` placeholder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
    /// `release`: the URL of the `.sha256` checksum file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_url: Option<String>,
    /// `release`: an inline sha256 checksum, as an alternative to `checksum_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// `git`: the repository URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// `git`: the branch / tag / commit to check out.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// `local`: the path to the already-built artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The symlink an activate swaps to point at the newly-staged artifact (the
    /// host's "current" path). Required at activate time for `release`/`local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_path: Option<PathBuf>,
    /// Where versioned artifacts are staged before activation. Defaults to
    /// `<workdir>/.fraisier-staging` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staging_dir: Option<PathBuf>,
}

/// The `[migration]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationSection {
    /// The migration adapter name (`"confiture"`, `"command"`, or an IPC
    /// adapter discovered on `PATH`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// The *source* env var holding the database DSN. Mapped onto the logical
    /// `DATABASE_URL` secret (Decision 5); the value itself never enters config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_url_env: Option<String>,
    /// The migrations directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrations_path: Option<PathBuf>,
    /// Whether to run the adapter's forward-compatibility `preflight` lint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_compatible_lint: Option<bool>,
    /// Adapter-specific settings (the `[migration.settings]` sub-table), passed
    /// through verbatim to `AdapterCtx.settings`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, serde_json::Value>,
}

/// The `[service]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSection {
    /// The service-manager adapter (`"systemd"`, `"rc"`, `"docker-compose"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// `systemd`: the unit name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// `rc`: the rc.d service name (the `service <name> …` argument).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The `[health]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthSection {
    /// The health adapter (`"http"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// `http`: the probe URL (may contain a `{host.address}` placeholder
    /// substituted per host at deploy time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `http`: the expected status code (defaults to `200`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_status: Option<u16>,
}

/// The `[lb]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LbSection {
    /// The load-balancer adapter (`"nginx"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// `nginx`: the path to the site config to rewrite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<PathBuf>,
    /// `nginx`: the upstream block to manage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

impl DeployConfig {
    /// Resolve the `[hosts]` strategy into the frozen [`RolloutStrategy`].
    ///
    /// Returns `None` for a single-host config (no `[hosts]`). Assumes the
    /// config has validated clean; an unrecognised strategy string falls back to
    /// `Rolling` (validation reports it as an error rather than silently picking
    /// a strategy here).
    #[must_use]
    pub fn rollout_strategy(&self) -> Option<RolloutStrategy> {
        let hosts = self.hosts.as_ref()?;
        let strategy = match hosts.strategy.as_deref() {
            Some(s) if STRATEGY_ALL_AT_ONCE.contains(&s) => RolloutStrategy::AllAtOnce,
            _ => RolloutStrategy::Rolling(hosts.rolling_batch_size.unwrap_or(1)),
        };
        Some(strategy)
    }

    /// Resolve the `[hosts]` inventory into the frozen [`HostInventory`].
    ///
    /// Returns `None` for a single-host config (no `[hosts]`).
    #[must_use]
    pub fn host_inventory(&self) -> Option<HostInventory> {
        let hosts = self.hosts.as_ref()?;
        let mut inventory = HostInventory::new();
        for spec in &hosts.inventory {
            let name = spec.name.clone().unwrap_or_default();
            let address = spec.address.clone().unwrap_or_default();
            inventory = inventory.with_host(HostEntry::new(HostId::new(name), address));
        }
        Some(inventory)
    }

    /// The logical secret mapping for the migration axis (Decision 5):
    /// `{"DATABASE_URL" → <database_url_env>}`, or empty when no DSN env is set.
    #[must_use]
    pub fn migration_env_secrets(&self) -> BTreeMap<String, String> {
        let mut secrets = BTreeMap::new();
        if let Some(source) = self
            .migration
            .as_ref()
            .and_then(|m| m.database_url_env.clone())
        {
            secrets.insert(DATABASE_URL_LOGICAL.to_owned(), source);
        }
        secrets
    }

    /// Build the base [`AdapterCtx`] for the migration axis.
    ///
    /// It carries the fraise/environment identity, the migrations path, the
    /// adapter settings, and the Decision-5 `env_secrets` mapping. The CLI fills
    /// the per-deploy fields (`host`, `workdir`, `previous_revision`,
    /// `artifact_ref`) at execution time.
    #[must_use]
    pub fn migration_adapter_ctx(&self) -> AdapterCtx {
        let deploy = self.deploy.as_ref();
        let fraise = deploy.and_then(|d| d.name.clone()).unwrap_or_default();
        let environment = deploy
            .and_then(|d| d.environment.clone())
            .unwrap_or_default();

        let mut ctx = AdapterCtx::new(fraise, environment);
        if let Some(migration) = &self.migration {
            ctx.migrations_path.clone_from(&migration.migrations_path);
            ctx.settings.clone_from(&migration.settings);
        }
        ctx.env_secrets = self.migration_env_secrets();
        ctx
    }

    /// The health probe's expected status code, defaulting to `200`.
    #[must_use]
    pub fn health_expected_status(&self) -> u16 {
        self.health
            .as_ref()
            .and_then(|h| h.expected_status)
            .unwrap_or(DEFAULT_EXPECTED_STATUS)
    }
}

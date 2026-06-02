//! The `[specql]` preset (PRD §7.1a) and the explicit-wins merge it relies on.
//!
//! The preset is a *config-time* expansion, not a runtime layer: at load time it
//! is turned into a full [`DeployConfig`] of Fraise-stack-conventional defaults,
//! and the user's explicit fields are then overlaid on top so that anything the
//! user wrote wins, field by field.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::schema::{
    ArtifactSection, DeploySection, HealthSection, HostSpec, HostsSection, LbSection,
    MigrationSection, ServiceSection, DEFAULT_EXPECTED_STATUS, STRATEGY_ROLLING,
};
use crate::DeployConfig;

/// The conventional migration adapter for a SpecQL-deployed app.
const PRESET_MIGRATION_ADAPTER: &str = "confiture";
/// The conventional service manager.
const PRESET_SERVICE_ADAPTER: &str = "systemd";
/// The conventional health adapter.
const PRESET_HEALTH_ADAPTER: &str = "http";
/// The conventional load-balancer adapter.
const PRESET_LB_ADAPTER: &str = "nginx";
/// The conventional artifact source: a locally-built binary.
const PRESET_ARTIFACT_SOURCE: &str = "local";
/// Where SpecQL apps build their release artifact by default.
const PRESET_ARTIFACT_PATH: &str = "./target/release";
/// The conventional source env var name for the database DSN.
const PRESET_DATABASE_URL_ENV: &str = "DATABASE_URL";
/// The conventional health-probe URL template (`{host.address}` is substituted
/// per host at deploy time).
const PRESET_HEALTH_URL: &str = "http://{host.address}:8080/health";

/// The one-line `[specql]` preset for a SpecQL-deployed app (PRD §7.1a).
///
/// It expands at load time into a full [`DeployConfig`]; explicit blocks the user
/// writes alongside it override the corresponding preset fields.
///
/// # Example
/// ```
/// use fraisier_config::DeployConfig;
///
/// let cfg = DeployConfig::from_toml_str(
///     r#"
///     [specql]
///     name = "app"
///     schema = "./schema.toml"
///     environment = "production"
///     hosts = ["a.internal", "b.internal"]
///     "#,
/// )
/// .expect("parses");
/// assert_eq!(cfg.service.unwrap().unit.as_deref(), Some("app.service"));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecqlPreset {
    /// The deployable's name. Required because `[deploy].name` is required and
    /// reading it from the SpecQL `schema` is deferred past Phase 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The path to the SpecQL `schema.toml`. Recorded but not read in Phase 1;
    /// it only seeds the default `migrations_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<PathBuf>,
    /// The target environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// The host addresses. More than one expands into a multi-host rollout with
    /// an nginx load balancer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
}

impl SpecqlPreset {
    /// Expand the preset into a full [`DeployConfig`] of conventional defaults.
    #[must_use]
    pub fn expand(&self) -> DeployConfig {
        let name = self.name.clone();
        let multi_host = self.hosts.len() > 1;

        DeployConfig {
            deploy: Some(DeploySection {
                name: name.clone(),
                environment: self.environment.clone(),
            }),
            hosts: multi_host.then(|| self.host_defaults()),
            artifact: Some(ArtifactSection {
                source: Some(PRESET_ARTIFACT_SOURCE.to_owned()),
                path: Some(PathBuf::from(PRESET_ARTIFACT_PATH)),
                ..ArtifactSection::default()
            }),
            migration: Some(MigrationSection {
                adapter: Some(PRESET_MIGRATION_ADAPTER.to_owned()),
                database_url_env: Some(PRESET_DATABASE_URL_ENV.to_owned()),
                migrations_path: Some(self.migrations_path()),
                forward_compatible_lint: Some(true),
                ..MigrationSection::default()
            }),
            service: Some(ServiceSection {
                adapter: Some(PRESET_SERVICE_ADAPTER.to_owned()),
                unit: name.as_ref().map(|n| format!("{n}.service")),
            }),
            health: Some(HealthSection {
                adapter: Some(PRESET_HEALTH_ADAPTER.to_owned()),
                url: Some(PRESET_HEALTH_URL.to_owned()),
                expected_status: Some(DEFAULT_EXPECTED_STATUS),
            }),
            lb: multi_host.then(|| Self::lb_defaults(name.as_deref())),
            specql: None,
        }
    }

    /// The default `migrations_path`: the schema file's directory + `migrations`.
    fn migrations_path(&self) -> PathBuf {
        let dir = self
            .schema
            .as_deref()
            .and_then(Path::parent)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        dir.join("migrations")
    }

    /// The default `[hosts]` block: a rolling(1) rollout over the listed
    /// addresses (host name == address).
    fn host_defaults(&self) -> HostsSection {
        HostsSection {
            strategy: Some(STRATEGY_ROLLING.to_owned()),
            rolling_batch_size: Some(1),
            inventory: self
                .hosts
                .iter()
                .map(|address| HostSpec {
                    name: Some(address.clone()),
                    address: Some(address.clone()),
                })
                .collect(),
        }
    }

    /// The default `[lb]` block: nginx, with `<name>`-derived config path and
    /// upstream.
    fn lb_defaults(name: Option<&str>) -> LbSection {
        let name = name.unwrap_or("app");
        LbSection {
            adapter: Some(PRESET_LB_ADAPTER.to_owned()),
            config_path: Some(PathBuf::from(format!("/etc/nginx/sites-available/{name}"))),
            upstream: Some(format!("{name}_upstream")),
        }
    }
}

/// Overlay `over` onto `base`, returning a value where every field set in `over`
/// wins and the rest falls back to `base`. This is the "explicit wins" rule.
trait Overlay {
    /// Merge `over` on top of `self`.
    fn overlay(self, over: Self) -> Self;
}

impl<T> Overlay for Option<T>
where
    T: Overlay,
{
    fn overlay(self, over: Self) -> Self {
        match (self, over) {
            (Some(base), Some(over)) => Some(base.overlay(over)),
            (base, None) => base,
            (None, over) => over,
        }
    }
}

/// `prefer(base, over)`: a leaf field where `over` wins if it is set.
fn prefer<T>(base: Option<T>, over: Option<T>) -> Option<T> {
    over.or(base)
}

impl Overlay for DeploySection {
    fn overlay(self, over: Self) -> Self {
        Self {
            name: prefer(self.name, over.name),
            environment: prefer(self.environment, over.environment),
        }
    }
}

impl Overlay for HostsSection {
    fn overlay(self, over: Self) -> Self {
        Self {
            strategy: prefer(self.strategy, over.strategy),
            rolling_batch_size: prefer(self.rolling_batch_size, over.rolling_batch_size),
            // A list can't be field-merged: an explicit inventory replaces the
            // preset's wholesale; an empty one keeps the preset's.
            inventory: if over.inventory.is_empty() {
                self.inventory
            } else {
                over.inventory
            },
        }
    }
}

impl Overlay for ArtifactSection {
    fn overlay(self, over: Self) -> Self {
        // An explicit `source` selects a different artifact strategy, so when the
        // user names one the preset's source-specific fields must not bleed
        // through. Otherwise merge field by field.
        if over.source.is_some() && over.source != self.source {
            return over;
        }
        Self {
            source: prefer(self.source, over.source),
            release_url: prefer(self.release_url, over.release_url),
            checksum_url: prefer(self.checksum_url, over.checksum_url),
            checksum: prefer(self.checksum, over.checksum),
            repo: prefer(self.repo, over.repo),
            reference: prefer(self.reference, over.reference),
            path: prefer(self.path, over.path),
            active_path: prefer(self.active_path, over.active_path),
            staging_dir: prefer(self.staging_dir, over.staging_dir),
        }
    }
}

impl Overlay for MigrationSection {
    fn overlay(self, over: Self) -> Self {
        let mut settings = self.settings;
        settings.extend(over.settings); // explicit keys win
        Self {
            adapter: prefer(self.adapter, over.adapter),
            database_url_env: prefer(self.database_url_env, over.database_url_env),
            migrations_path: prefer(self.migrations_path, over.migrations_path),
            forward_compatible_lint: prefer(
                self.forward_compatible_lint,
                over.forward_compatible_lint,
            ),
            settings,
        }
    }
}

impl Overlay for ServiceSection {
    fn overlay(self, over: Self) -> Self {
        Self {
            adapter: prefer(self.adapter, over.adapter),
            unit: prefer(self.unit, over.unit),
        }
    }
}

impl Overlay for HealthSection {
    fn overlay(self, over: Self) -> Self {
        Self {
            adapter: prefer(self.adapter, over.adapter),
            url: prefer(self.url, over.url),
            expected_status: prefer(self.expected_status, over.expected_status),
        }
    }
}

impl Overlay for LbSection {
    fn overlay(self, over: Self) -> Self {
        Self {
            adapter: prefer(self.adapter, over.adapter),
            config_path: prefer(self.config_path, over.config_path),
            upstream: prefer(self.upstream, over.upstream),
        }
    }
}

impl DeployConfig {
    /// Overlay this (user-written, explicit) config on top of `base` (the
    /// expanded preset), so that explicit fields win.
    pub(crate) fn overlay_onto(self, base: Self) -> Self {
        Self {
            deploy: base.deploy.overlay(self.deploy),
            hosts: base.hosts.overlay(self.hosts),
            artifact: base.artifact.overlay(self.artifact),
            migration: base.migration.overlay(self.migration),
            service: base.service.overlay(self.service),
            health: base.health.overlay(self.health),
            lb: base.lb.overlay(self.lb),
            specql: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SpecqlPreset;

    fn preset() -> SpecqlPreset {
        SpecqlPreset {
            name: Some("fraiseql".to_owned()),
            schema: Some("db/schema.toml".into()),
            environment: Some("production".to_owned()),
            hosts: vec!["a.internal".to_owned(), "b.internal".to_owned()],
        }
    }

    #[test]
    fn expands_migrations_path_relative_to_the_schema_dir() {
        let cfg = preset().expand();
        let path = cfg.migration.unwrap().migrations_path.unwrap();
        assert_eq!(path, std::path::PathBuf::from("db/migrations"));
    }

    #[test]
    fn single_host_preset_omits_hosts_and_lb() {
        let mut p = preset();
        p.hosts = vec!["only.internal".to_owned()];
        let cfg = p.expand();
        assert!(cfg.hosts.is_none());
        assert!(cfg.lb.is_none());
    }

    #[test]
    fn explicit_artifact_source_replaces_preset_local_default() {
        let base = preset().expand();
        let over = crate::DeployConfig {
            artifact: Some(crate::schema::ArtifactSection {
                source: Some("release".to_owned()),
                release_url: Some("https://example.com/app.tar.gz".to_owned()),
                ..crate::schema::ArtifactSection::default()
            }),
            ..crate::DeployConfig::default()
        };
        let merged = over.overlay_onto(base);
        let artifact = merged.artifact.unwrap();
        assert_eq!(artifact.source.as_deref(), Some("release"));
        // The preset's `local` path must not bleed into a release artifact.
        assert!(artifact.path.is_none());
    }
}

//! # fraisier-config
//!
//! Parser, **SpecQL-preset** expander, and validator for the native
//! `fraisier.toml` deploy configuration (PRD §7.1 / §7.1a).
//!
//! This crate is the bridge between the config file and the **frozen** vocabulary
//! types in [`fraisier-core`]. It depends on `fraisier-core` for those types only
//! and **never** on `fraisier-ipc` (the crate-graph rule); concrete adapter
//! selection is wired at the CLI / embedder layer.
//!
//! ## Three stages
//!
//! 1. **Parse** — [`DeployConfig::from_toml_str`] deserializes the seven
//!    sections (`[deploy]`, `[hosts]`, `[artifact]`, `[migration]`, `[service]`,
//!    `[health]`, `[lb]`). Unknown keys are rejected with a located error.
//! 2. **Expand** — if a `[specql]` preset is present it is expanded into a full
//!    config of Fraise-stack defaults, with the user's explicit fields overlaid
//!    on top (explicit wins). See [`SpecqlPreset`].
//! 3. **Validate** — [`DeployConfig::validate`] is a *separate*, I/O-free pass
//!    that judges value-domain and per-adapter requirements, returning every
//!    located issue in a [`ValidationReport`].
//!
//! [`DeployConfig::load`] runs all three and fails on any `Error`-severity issue.
//!
//! ## Secret handling (Decision 5)
//!
//! `[migration].database_url_env` names the *source* env var holding the database
//! DSN. The config maps it onto the logical secret `DATABASE_URL` in
//! [`AdapterCtx::env_secrets`](fraisier_core::adapter_axes::AdapterCtx::env_secrets)
//! (see [`DeployConfig::migration_adapter_ctx`]); the value itself never enters
//! the config file, argv, or JSON params.
//!
//! ## Example
//!
//! ```
//! use fraisier_config::DeployConfig;
//!
//! let cfg = DeployConfig::load(
//!     r#"
//!     [deploy]
//!     name = "fraiseql"
//!     environment = "production"
//!
//!     [artifact]
//!     source = "release"
//!     release_url = "https://example.com/app-{version}.tar.gz"
//!     checksum_url = "https://example.com/app-{version}.tar.gz.sha256"
//!
//!     [migration]
//!     adapter = "confiture"
//!     database_url_env = "FRAISEQL_DATABASE_URL"
//!
//!     [service]
//!     adapter = "systemd"
//!     unit = "fraiseql.service"
//!
//!     [health]
//!     adapter = "http"
//!     url = "http://127.0.0.1:8080/health"
//!     "#,
//! )
//! .expect("valid config");
//!
//! assert_eq!(cfg.migration_env_secrets()["DATABASE_URL"], "FRAISEQL_DATABASE_URL");
//! ```

pub mod calendar;
mod error;
mod preset;
mod schema;
mod validate;

pub use error::ConfigError;
pub use preset::SpecqlPreset;
pub use schema::{
    ArtifactSection, BlueGreenSection, CheckSection, DeployConfig, DeploySection, HealthSection,
    HostSpec, HostsSection, LbSection, MigrationSection, PolicySection, ScheduleSection,
    ServiceSection, SshSection, DEFAULT_APPROVAL_TIMEOUT_SECS, UNCLASSIFIED_ACTIONS,
};
pub use validate::{ValidationIssue, ValidationReport};

// Re-exported so callers can match on issue severity without depending on
// `fraisier-core` directly.
pub use fraisier_core::adapter_axes::Severity;

impl DeployConfig {
    /// Parse a `fraisier.toml`, expanding the `[specql]` preset if present.
    ///
    /// This does **not** validate; call [`DeployConfig::validate`] (or use
    /// [`DeployConfig::load`]) for the semantic pass.
    ///
    /// # Errors
    /// [`ConfigError::Parse`] if the TOML is malformed, has a wrong-typed value,
    /// or contains an unknown key.
    pub fn from_toml_str(toml: &str) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(toml)?;
        if let Some(preset) = config.specql.take() {
            config = config.overlay_onto(preset.expand());
        }
        Ok(config)
    }

    /// Parse, expand the preset, and validate in one step.
    ///
    /// # Errors
    /// [`ConfigError::Parse`] if the TOML is malformed, or [`ConfigError::Invalid`]
    /// carrying the [`ValidationReport`] if validation finds any `Error`-severity
    /// issue.
    pub fn load(toml: &str) -> Result<Self, ConfigError> {
        let config = Self::from_toml_str(toml)?;
        let report = config.validate();
        if report.ok() {
            Ok(config)
        } else {
            Err(ConfigError::Invalid(report))
        }
    }
}

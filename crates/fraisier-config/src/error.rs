//! The crate's error type.

use crate::validate::ValidationReport;

/// An error from loading a [`DeployConfig`](crate::DeployConfig).
///
/// Two failure modes, deliberately distinct so callers can tell a malformed file
/// from a well-formed-but-invalid one:
///
/// - [`ConfigError::Parse`] — the TOML is syntactically wrong or a value has the
///   wrong type / an unknown key. The wrapped [`toml::de::Error`] carries the
///   line and column.
/// - [`ConfigError::Invalid`] — the TOML parsed, but the configuration failed the
///   semantic [validation pass](crate::DeployConfig::validate). The wrapped
///   [`ValidationReport`] lists every located issue.
///
/// # Example
/// ```
/// use fraisier_config::{ConfigError, DeployConfig};
///
/// match DeployConfig::load("not = valid = toml") {
///     Err(ConfigError::Parse(_)) => {}
///     other => panic!("expected a parse error, got {other:?}"),
/// }
/// ```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The TOML could not be parsed (syntax, wrong type, or unknown field).
    #[error("could not parse fraisier config: {0}")]
    Parse(#[from] toml::de::Error),

    /// The config parsed but failed validation. See the report for located
    /// issues.
    #[error("invalid fraisier config:\n{0}")]
    Invalid(ValidationReport),
}

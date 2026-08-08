# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- *(adapter)* stop trusting `window_safe` from a confiture that cannot classify.
  The capability is now advertised only for **confiture ≥ 0.44.0**, where it was
  previously hard-coded into the static capability list and claimed for every
  version. Up to 0.43.0 confiture's replica classifier recognised four AST node
  types and returned nothing for the rest, and `window_safe` is derived from the
  *absence* of `PFLIGHT_REPLICA_*` findings — so a `DROP TABLE` it could not read
  reported `window_safe: true` (fraiseql/confiture#206). Because fraisier's
  blue-green baseline refuses a *missing* verdict but admits a present one, that
  let a migration no two-version window survives pass the gate.

  **This refuses deploys that previously succeeded.** A blue-green deploy backed
  by confiture 0.23.0–0.43.0 is now denied at preflight, naming the missing
  capability. Upgrading confiture to ≥ 0.44.0 restores it. Single-host and
  multi-host deploys are unaffected — they never ran the window-safety baseline.

## [1.0.0-beta.7](https://github.com/fraiseql/fraisier-core/compare/fraisier-adapter-confiture-v1.0.0-beta.6...fraisier-adapter-confiture-v1.0.0-beta.7) - 2026-08-06

### Added

- *(adapter)* parse the confiture change-set, advertise risk_tier honestly
- *(adapter)* carry the schema risk tier across the migration seam

### Fixed

- *(confiture)* withhold risk_tier — no released confiture can classify

### Other

- *(release)* 1.0.0-beta.7
- *(contract)* specify the migration risk contract + golden fixtures

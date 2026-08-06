# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-beta.7](https://github.com/fraiseql/fraisier-core/compare/fraisier-core-v1.0.0-beta.6...fraisier-core-v1.0.0-beta.7) - 2026-08-06

### Added

- *(preview)* the dry-run reads the schema and says what it cannot see
- *(policy)* audit the decision, warn on the silent-deny config, document the gate
- *(policy)* one gate on all three strategies — the window-safety module is gone
- *(policy)* the gate stops a single-host deploy, and an approval hook can unblock it
- *(policy)* one decision function — risk-keyed policy + window-safety baseline
- *(adapter)* parse the confiture change-set, advertise risk_tier honestly
- *(adapter)* carry the schema risk tier across the migration seam

### Other

- *(release)* 1.0.0-beta.7

## [1.0.0-beta.6](https://github.com/fraiseql/fraisier-core/compare/fraisier-core-v1.0.0-beta.5...fraisier-core-v1.0.0-beta.6) - 2026-07-23

### Fixed

- *(command)* run migrations from the staged release directory ([#38](https://github.com/fraiseql/fraisier-core/pull/38))

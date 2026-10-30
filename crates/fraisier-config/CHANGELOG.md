# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-beta.15](https://github.com/fraiseql/fraisier-core/compare/fraisier-config-v1.0.0-beta.13...fraisier-config-v1.0.0-beta.15) - 2026-10-30

### Added

- *(scaffold)* scheduled refresh unit with alert, --skip-if-locked
- *(config)* [refresh].units, the environment's other units
- *(config)* [refresh] section and explicit [deploy] production

### Other

- release v1.0.0-beta.14

## [1.0.0-beta.14](https://github.com/fraiseql/fraisier-core/compare/fraisier-config-v1.0.0-beta.13...fraisier-config-v1.0.0-beta.14) - 2026-10-30

### Added

- *(scaffold)* scheduled refresh unit with alert, --skip-if-locked
- *(config)* [refresh].units, the environment's other units
- *(config)* [refresh] section and explicit [deploy] production

## [1.0.0-beta.13](https://github.com/fraiseql/fraisier-core/compare/fraisier-config-v1.0.0-beta.12...fraisier-config-v1.0.0-beta.13) - 2026-10-29

### Fixed

- *(config)* a retain entry's name is checked once, against one rule

### Other

- Merge remote-tracking branch 'origin/main' into HEAD
- Merge remote-tracking branch 'origin/main' into HEAD

## [1.0.0-beta.12](https://github.com/fraiseql/fraisier-core/compare/fraisier-config-v1.0.0-beta.11...fraisier-config-v1.0.0-beta.12) - 2026-10-27

### Fixed

- *(preflight)* honour preflight_mode on multi-host and db migrate
- *(saga)* a database-less deployable rolls back cleanly (adapter = "none")
- *(lint)* pass clippy on Rust 1.99 (unblocks #65/#76 for #63)

### Other

- Merge main into chore/strip-plan-labels
- Merge main into fix/retain-prune-scope
- Merge main into fix/preflight-mode-multi-host
- Merge main into feat/85-stack
- Merge main into feat/84-alerts
- Merge main into feat/86-metrics
- Merge main into feat/84-drill
- Merge main into feat/84-retention
- Merge main into feat/88-tviews-gate
- *(config)* rustfmt the merged re-export list
- Merge main into feat/83-pg-readiness
- Merge main into feat/82-require-capabilities
- Merge main into fix/no-database-migration
- *(ci)* gate rustdoc, and fix the nine errors that had accumulated

## [1.0.0-beta.11](https://github.com/fraiseql/fraisier-core/compare/fraisier-config-v1.0.0-beta.10...fraisier-config-v1.0.0-beta.11) - 2026-09-26

### Other

- release v1.0.0-beta.10

## [1.0.0-beta.10](https://github.com/fraiseql/fraisier-core/compare/fraisier-config-v1.0.0-beta.9...fraisier-config-v1.0.0-beta.10) - 2026-09-26

### Other

- release v1.0.0-beta.10

## [1.0.0-beta.8](https://github.com/fraiseql/fraisier-core/compare/fraisier-config-v1.0.0-beta.7...fraisier-config-v1.0.0-beta.8) - 2026-08-08

### Other

- *(release)* 1.0.0-beta.8
- release v1.0.0-beta.7

## [1.0.0-beta.7](https://github.com/fraiseql/fraisier-core/compare/fraisier-config-v1.0.0-beta.6...fraisier-config-v1.0.0-beta.7) - 2026-08-06

### Added

- *(policy)* audit the decision, warn on the silent-deny config, document the gate
- *(policy)* the gate stops a single-host deploy, and an approval hook can unblock it
- *(policy)* one decision function — risk-keyed policy + window-safety baseline

### Other

- *(release)* 1.0.0-beta.7

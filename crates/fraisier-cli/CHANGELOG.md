# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-beta.8](https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.7...fraisier-v1.0.0-beta.8) - 2026-08-06

### Other

- release v1.0.0-beta.7

## [1.0.0-beta.7](https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.6...fraisier-v1.0.0-beta.7) - 2026-08-06

### Added

- *(preview)* blue-green's plan reaches parity, window-safety verdict included
- *(preview)* the plan reports the policy verdict, and CI can gate on it
- *(preview)* the dry-run reads the schema and says what it cannot see
- *(policy)* audit the decision, warn on the silent-deny config, document the gate
- *(policy)* one gate on all three strategies — the window-safety module is gone
- *(policy)* the gate stops a single-host deploy, and an approval hook can unblock it

### Other

- *(release)* 1.0.0-beta.7
- *(preview)* the plan is documented, and the docs cannot drift from it

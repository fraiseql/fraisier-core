# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-beta.4](https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.3...fraisier-v1.0.0-beta.4) - 2026-06-22

### Added

- *(cli)* scheduled-install drift/prune; compact --verbose
- *(cli)* doctor + env-check; validate-config --resolve-envvars
- *(webhook)* self-upgrade drain (503 + Retry-After)
- *(health)* smoke-test token providers for authed probes
- *(ship)* --no-bump reship + version-race detection
- *(preflight)* restore-rehearsal migration preflight (DR-grade)

### Other

- *(release)* prepare 1.0.0-beta.4
- finalize gap-bridging — PRD, CHANGELOG, security non-ports

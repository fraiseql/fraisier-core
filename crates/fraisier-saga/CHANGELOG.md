# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-beta.12](https://github.com/fraiseql/fraisier-core/compare/fraisier-saga-v1.0.0-beta.11...fraisier-saga-v1.0.0-beta.12) - 2026-09-26

### Other

- *(ci)* gate rustdoc, and fix the nine errors that had accumulated

## [1.0.0-beta.10](https://github.com/fraiseql/fraisier-core/compare/fraisier-saga-v1.0.0-beta.9...fraisier-saga-v1.0.0-beta.10) - 2026-09-26

### Fixed

- *(security)* redact credentials on the three paths that bypass the CLI

### Other

- *(saga)* one credential-redaction helper, reachable from the engine

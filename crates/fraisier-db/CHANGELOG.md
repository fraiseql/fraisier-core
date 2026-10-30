# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-beta.13](https://github.com/fraiseql/fraisier-core/compare/fraisier-db-v1.0.0-beta.12...fraisier-db-v1.0.0-beta.13) - 2026-10-29

### Fixed

- *(db)* pass the restore target to pg_restore as a quoted dbname pair

### Other

- Merge remote-tracking branch 'origin/main' into HEAD

## [1.0.0-beta.12](https://github.com/fraiseql/fraisier-core/compare/fraisier-db-v1.0.0-beta.11...fraisier-db-v1.0.0-beta.12) - 2026-10-27

### Added

- *(db)* one restore primitive in fraisier-db
- *(db)* [database.requires] — check PostgreSQL major and pg_tviews readiness, refuse an unready server

### Fixed

- *(rehearsal)* the teardown never drops the live database

### Other

- *(db)* one free-space rule, in the restore primitive
- Merge feat/06c-restore-primitive (and main) into feat/56-deep-classification
- Merge main into feat/84-retention
- Merge main into feat/88-tviews-gate
- Merge main into feat/83-pg-readiness

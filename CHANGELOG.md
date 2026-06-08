# Changelog

All notable changes to fraisier are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-beta.2] - 2026-06-08

### Added

- **Project checks** (`fraisier check` + `fraisier-check` crate): a declarative
  `[[checks]]` list in `fraisier.toml` — named shell commands run via `sh -c`
  with cross-check parallelism (`-j`, default auto). One source of truth runnable
  locally and in CI. A check passes iff its command exits `0`; output is captured
  and shown for failures (and always under `--json`).
- **`ship` check gate**: `fraisier ship` runs `[[checks]]` before the version
  bump and refuses to release if any fails. On by default; `--no-check` skips it;
  a project with no `[[checks]]` ships exactly as before; `--dry-run` reports the
  checks would run without executing them.

## [1.0.0-beta.1]

The first public release: a deploy-orchestration engine with atomic,
migration-safe rollback across one host or a fleet.

### Engine

- **Saga engine** (`fraisier-saga`): ordered compensable steps with reverse-order
  rollback and a durable release ledger; outcomes are committed, rolled back, or
  partially rolled back. The `Saga` / `Step` / `StepContext` / `SagaError` /
  `SagaOutcome` API is the frozen v1.0 contract.
- **Pluggable state store**: filesystem, sqlite, and in-memory backends behind
  one `StateStore` trait, with key enumeration.
- **Five adapter axes** — `artifact`, `migration`, `service`, `health`, `lb` —
  each usable in-process (Rust trait) or out-of-process over a JSON-RPC-over-stdio
  protocol, so adapters can be written in any language. Adapters receive secret
  names, never values.

### Deploy flows

- **Single-host deploy**: `preflight → fetch → migrate → activate → restart →
  health → verify`, with atomic rollback (prior release re-activated from the
  ledger, migration rolled down) on any failure.
- **Multi-host deploy**: rolling and all-at-once rollouts across an SSH fleet
  (shell-out or IPC transport with OpenSSH `ControlMaster` reuse); migrate once on
  the orchestrator, per-host rollout with load-balancer drain/reattach, reverse-
  order fleet rollback.
- **Blue-green deploy**: a forward-compatibility window-safety hard gate
  (consuming confiture's first-class `window_safe` verdict, confiture ≥ 0.23.0), a
  pre-swap connection-budget probe, an nginx traffic swap, and swap-back to the
  still-hot blue fleet if green degrades during the hold (no database rollback).

### Adapters

- artifact: release tarball, git, local path, host-pull, IPC.
- migration: confiture (DSN via environment + `--no-config`), command, IPC.
- service: systemd, rc, docker-compose.
- health: http.
- lb: nginx (upstream-include symlink swap).

### CLI & operations

- `init`, `validate-config`, `deploy` (`--dry-run`, `--host`, `--app-version`),
  `list`, `status` (`--per-host`), `health`, `rollback`.
- `bootstrap` (prepare host directories over SSH/locally).
- `webhook-server`: HMAC-signed, replay-protected deploy trigger, socket-activated
  or standalone.
- `scheduled` (systemd timer/service install + CRUD) with an unattended-deploy
  safety gate and failure notification.
- `self-upgrade` (coordinated graceful restart).
- `sync` (share the deploy ledger across operators over git refs; experimental).
- `backup` and `db` (migrate / restore / reset) for generic-Postgres lifecycle.
- `providers` / `provider-test` (enumerate and probe adapters per axis).
- `version` / `ship` (version bump, commit, push, deploy).
- `scaffold` / `scaffold-install` (generate and install systemd/socket/nginx/CI
  files).

### Observability & supply chain

- Every saga step is exported as an OpenTelemetry span and persisted to the state
  store.
- Workspace-wide `unsafe_code = "forbid"`; `cargo deny` gate over advisories,
  licenses, bans, and sources.

### Developer tooling

- **Task runner** (`crates/xtask`, zero dependencies): `cargo xtask ci` runs the
  full gate (fmt, clippy `-D warnings`, test, release build, `cargo deny`,
  shellcheck) — the same command CI invokes, so local and CI cannot drift.
  `cargo xtask dist` cross-builds the static musl binary via `cargo-zigbuild`.
- **GitHub Actions CI** that runs `cargo xtask ci`.

[Unreleased]: https://github.com/fraiseql/fraisier-core/compare/v1.0.0-beta.1...HEAD
[1.0.0-beta.1]: https://github.com/fraiseql/fraisier-core/releases/tag/v1.0.0-beta.1

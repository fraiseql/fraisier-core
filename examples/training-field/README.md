# Training field — a real Confiture-on-Postgres deploy fixture

A deliberately tiny, **DB-backed** app and deploy config used to exercise the
*real* fraisier deploy pipeline end-to-end with the **in-process Confiture
migration adapter** against a **real Postgres**.

It exists because every other end-to-end path we have — `checkpoint-local.sh`,
`checkpoint-matrix.sh`, the Hetzner run — drives the **sqlx** adapter over IPC
against **SQLite**. The Confiture adapter is wired into the factory
(`adapter = "confiture"`) and unit/integration-tested standalone, but until this
fixture nothing exercised it *inside an actual deploy saga* (artifact staging →
migrate → activate → restart → health → verify → rollback) against real Postgres.

It is **test scaffolding, not shipped** (excluded from the workspace,
`publish = false`). It is also **not** PRD §10.3 criterion 1 — that names
fraiseql v2. This is the training field that proves the engine + Confiture path
before the heavier real-fraiseql deploy.

## What it is

- `src/main.rs` — a ~100-line Tokio app. Health is a function of the active
  release's version name (read off the `current` symlink, as a real app reads its
  build) **and** the database:
  - `crash` in the version → exits before readiness (restart-phase failure);
  - `sick` in the version → serves HTTP 500 (health-phase failure);
  - otherwise serves 200 **iff** the Confiture-migrated `notes` table is queryable.
  Readiness is signalled via sd_notify (`Type=notify`).
- `migrations/` — Confiture migrations (`001_create_notes`, `002_add_label`),
  kept plainly forward-safe so the in-saga preflight lint passes.
- `fraisier.toml.example` — the deploy config shape (Confiture + systemd + HTTP
  health + a `release` artifact).

## How to run

`scripts/checkpoint-training.sh` builds the app, provisions a throwaway Postgres,
generates the runtime `fraisier.toml`, and drives the real engine: three
consecutive deploys plus forced migrate / restart / health failures, each
asserted to roll back to the healthy baseline. Run it locally
(`--systemd user`, a local Postgres container, zero spend) or on a remote host
via `scripts/checkpoint-hetzner.sh --training`.

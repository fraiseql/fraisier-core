# fraisier-adapter-confiture

The in-process **Confiture** migration adapter for fraisier. It implements the
frozen `MigrationAdapter` trait by wrapping the `confiture migrate <subcommand>`
CLI — the native, intimate-integration migration path of the FraiseQL stack
(PRD §6.3), distinct from the IPC subprocess adapters.

## Requirements

- **Confiture ≥ 0.20.0** on `PATH` (override with `FRAISIER_CONFITURE_BIN`) for
  `current` / `up` / `down-to` / `verify`: 0.20.0 provides `migrate current`,
  `migrate down-to`, and the `--no-config` env-only DSN mode this adapter relies on.
- **Confiture ≥ 0.22.0** for `preflight` (and therefore for a default deploy, which
  runs the forward-compat lint): earlier versions reject the `--output` flag the
  adapter passes to every subcommand. 0.22 also froze its exit-code / JSON shapes as
  a stability contract aligned to this adapter.

## DSN handoff (secrets via env, never argv)

The adapter resolves the database DSN through `AdapterCtx::secret("DATABASE_URL")`
and passes it to Confiture by setting **`CONFITURE_DATABASE_URL`** on the child
process together with **`--no-config`**. Under `--no-config`, Confiture treats the
environment as the *sole* DSN source, so a stray `db/environments/*.yaml` in the
deploy workdir cannot shadow the operator's DSN. The DSN never appears in argv,
and the adapter honours the in-process ⇄ IPC convergence rule.

## Method mapping

| Trait method      | Confiture command                          |
|-------------------|--------------------------------------------|
| `describe`        | `confiture --version` (synthesised)        |
| `current_revision`| `migrate current --no-config --format json`|
| `up`              | `migrate up [--target] --no-config …`      |
| `down_to`         | `migrate down-to <rev> --no-config …`      |
| `verify`          | `migrate verify --no-config …`             |
| `preflight`       | `migrate preflight --no-config …`          |
| `post_migrate`    | trait no-op (Confiture has no such command)|

## Tests

Unit tests (argument construction, secret-not-in-argv, exit-code mapping, JSON
parsing) run with no external dependencies. Integration tests skip gracefully
when `confiture` is absent; the full Postgres round-trip runs only when
`FRAISIER_TEST_DATABASE_URL` points at a usable, empty database.

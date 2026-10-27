# `migrate preflight --scope` captures

Transcripts of what confiture emitted when asked to judge **only the pending
migrations** (`--scope pending`), beside the same run without the flag. They
are fraisier-side observations, not part of the cross-repo pact in
`../preflight/`: they pin what a given binary did, and the pending-scope floor
(`PENDING_SCOPE_MIN_CONFITURE`) rests on them. Do not edit them; replace one
only by capturing again and saying so here.

| File | What it shows |
|---|---|
| `all-1.35.0.json` | No `--scope`: `scope: "all"`. The change-set covers `001`–`003`, so the applied `DROP COLUMN` in `002` is `irreversible` and `window_safe` is `false`. |
| `pending-1.35.0.json` | `--scope pending` on the same database: `scope: "pending"`, only `003` (`additive`), `window_safe: true`. |
| `pending-no-ledger-1.35.0.json` | `--scope pending` on a database with no ledger: every migration is pending, so the change-set is the same as `all`. |
| `pending-against-1.35.0.json` | `--scope pending` together with `--against` (the deep-classification argv): only `003`. |

## Provenance

Captured 2026-10-26, for "judge only pending migrations" (D13 (i)).

| | |
|---|---|
| Producers | `fraiseql-confiture==1.34.0`, `==1.35.0` and `==1.36.0` from PyPI (Python 3.14), each in its own `uv` venv |
| Server | PostgreSQL 18.6, a scratch cluster with `scram-sha-256` auth |
| Seeding | `001` and `002` applied with `confiture migrate up` (1.36.0) to a `shop` database as role `app`; `003` left pending |
| Invocation | `migrate preflight --scope pending --no-config --format json --output <tmp> --migrations-dir <dir>`, with `CONFITURE_DATABASE_URL` naming the database: the argv and environment `plan()` builds. The `--against` capture adds `--against postgres://app@127.0.0.1:<port>/shop` with the password in `PGPASSWORD` |
| Corpus | `001` `CREATE TABLE tb_order (id integer PRIMARY KEY, qty integer NOT NULL, note varchar(200))` plus two rows; `002` `ALTER TABLE tb_order DROP COLUMN note`; `003` `ALTER TABLE tb_order ADD COLUMN placed_at timestamptz`. Every migration has a `down` file |

What the runs also showed:

- **1.34.0 has no `--scope`.** It exits 2 with `No such option: --scope` and
  writes no report, so passing the flag below the floor breaks every
  preflight. 1.35.0 is the first release that accepts it.
- 1.36.0 emits byte-identical reports to 1.35.0 for all four runs.
- The pending set is read from the ledger of the database in
  `CONFITURE_DATABASE_URL`. Under deep classification that is the throwaway,
  whose ledger is a copy of the live one.

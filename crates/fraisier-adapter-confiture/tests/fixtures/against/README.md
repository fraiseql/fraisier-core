# `migrate preflight --against` captures

Transcripts of what confiture emitted when it classified pending migrations
**against a seeded copy** of a database, the way the restore-rehearsal preflight
asks it to (#56). They are fraisier-side observations, not part of the
cross-repo pact in `../preflight/`: they pin what a given binary did, and the
deep-classification floor (`DEEP_CLASSIFICATION_MIN_CONFITURE`) rests on them.
Do not edit them; replace one only by capturing again and saying so here.

| File | What it shows |
|---|---|
| `0.43.0.json` | The two type changes classified against the copy by 0.43.0: still untiered. |
| `0.44.0.json` | The same corpus on 0.44.0: the widening is `lock_risky`, the narrowing `irreversible`. |
| `1.36.0.json` | The same corpus on 1.36.0 (the CI pin's release), same tiers. |
| `replay-failed-1.36.0.json` | A narrowing the copy's data cannot take: `PFLIGHT_REPLAY_FAILED` at error severity, exit 7. |

## Provenance

Captured 2026-10-26, for the restore-rehearsal deep classification (#56).

| | |
|---|---|
| Producers | `fraiseql-confiture[ast]==0.43.0` and `==0.44.0` (Python 3.12), and `==1.36.0` from the pinned `tools/confiture-requirements.txt` set (Python 3.14), each in its own `uv` venv |
| Server | PostgreSQL 18.6, a scratch cluster with `scram-sha-256` auth, so a password was really required |
| Seeding | migration `001` applied to a `shop` database as role `app`; a copy made with `pg_dump -Fc` → `CREATE DATABASE` → `pg_restore`, as the rehearsal provisions one |
| Invocation | `migrate preflight --no-config --format json --output <tmp> --migrations-dir <dir> --against postgres://app@127.0.0.1:55432/<copy>`, with `CONFITURE_DATABASE_URL` naming the same copy **with** its password and `PGPASSWORD` set: the argv and environment `plan()` builds for a deep target |
| Corpus | `001` `CREATE TABLE tb_order (id integer PRIMARY KEY, qty integer NOT NULL, note varchar(200))` plus two rows; `002` `ALTER COLUMN qty TYPE bigint`; `003` `ALTER COLUMN note TYPE varchar(20)`, or `varchar(2)` for the replay failure. Every migration has a `down` file |

What the run also showed, and the adapter relies on:

- One database works as both the env DSN and `--against`. The replay applies
  only what the copy's own ledger has not (`001` was not replayed), and it rolls
  back: afterwards the copy's ledger and column types were unchanged, so the
  rehearsal can migrate the same copy for real.
- Without `PGPASSWORD`, the password-less `--against` URL fails with `CONFIG_006`
  (exit 3, `fe_sendauth: no password supplied`), so libpq really does read the
  password from the environment.
- `change_set` still covers every migration (`scope: "all"`), applied ones
  included, because the adapter passes no `--scope`.

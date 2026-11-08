# Can `migrate down` undo it? — captures

Transcripts of `migrate preflight` on pending migrations that `migrate down` cannot
undo. The `reversibility` capability's floor (`REVERSIBILITY_MIN_CONFITURE`) rests on
them. Do not edit them; replace one only by capturing again and saying so here.

| File | What it shows |
|---|---|
| `down-empty-1.36.0.json` | A `.py` migration whose `down()` is `pass`: no `reversible`, no `PFLIGHT_DOWN_EMPTY`. |
| `down-empty-1.39.0.json` | The same on 1.39.0: still neither. |
| `down-empty-1.40.0.json` | 1.40.0: `reversible: false` and `PFLIGHT_DOWN_EMPTY` naming the migration. The floor. |
| `down-empty-1.45.0.json` | 1.45.0 (the CI pin): the same. |
| `missing-down-1.45.0.json` | A `.up.sql` with no `.down.sql`: `PFLIGHT_MISSING_DOWN` at error severity, exit 7. |

## Provenance

Captured 2026-11-07.

| | |
|---|---|
| Producers | `fraiseql-confiture==1.36.0`, `1.39.0`, `1.40.0` and `1.45.0`, each in its own `uv` venv (Python 3.14) |
| Server | PostgreSQL 18.4, a fresh empty database per version |
| Invocation | `migrate preflight --no-config --migrations-dir db/migrations --format json --output <tmp> --scope pending`, with `CONFITURE_DATABASE_URL` naming the database |
| Corpus | `20261107000001_base.up.sql` (`CREATE TABLE base (id int)`) with its `.down.sql`, and `20261107000002_add_widgets.py`, a `Migration` whose `up()` creates `widgets` and whose `down()` is `pass`. For `missing-down`, one `20261107000003_add_gadgets.up.sql` alone |

What the same run measured, and the gate relies on: after `migrate up`, `migrate
down-to 20261107000001` on **1.36.0 and 1.39.0 exits 0, deletes the `.py`
migration's ledger row and leaves `widgets` in place**. From 1.40.0 it refuses
with `ROLLBACK_600` (exit 8) and changes nothing.

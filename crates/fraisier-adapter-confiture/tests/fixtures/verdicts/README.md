# Verdicts that moved with the 1.36.0 → 1.45.0 pin

One static corpus classified by both releases, so the policy gate's change in
verdict is on record. Do not edit them; replace one only by capturing again and
saying so here.

| Migration | 1.36.0 | 1.45.0 |
|---|---|---|
| `…06` `DROP VIEW v_item; CREATE VIEW v_item AS …` | `destructive`, `additive` | `reversible`, `additive` |
| `…08` `ALTER TABLE … DROP CONSTRAINT ck_qty; ALTER TABLE … ADD CONSTRAINT ck_qty CHECK …` | `destructive`, `lock_risky` | `lock_risky`, `lock_risky` |
| every other migration | unchanged | unchanged |

A `[policy]` that denies `destructive` refused both on 1.36.0 and admits them on
1.45.0.

**Not in the corpus, on purpose:** the same CHECK swap written as **one**
statement (`ALTER TABLE t DROP CONSTRAINT c, ADD CONSTRAINT c CHECK (x > 0);`)
makes confiture 1.42.0 through 1.45.0 fail the whole preflight with
`INTERNAL_ERROR` ("list index out of range", exit 1). 1.40.0 and 1.41.0 classify
it. The deploy is refused because the check could not run; it never passes.

## Provenance

Captured 2026-11-07 with `fraiseql-confiture==1.36.0` and `==1.45.0` (Python
3.14), `migrate preflight --no-config --format json --output <tmp>
--migrations-dir db/migrations`, `CONFITURE_DATABASE_URL` unreachable (the static
path never connects). The corpus, one migration each with a down file:
`CREATE TABLE`, `RENAME COLUMN`, `CREATE INDEX`, `TRUNCATE`, `CREATE VIEW`, the
view recreate, `ADD CONSTRAINT … CHECK`, the two-statement CHECK swap, `DROP TABLE`.

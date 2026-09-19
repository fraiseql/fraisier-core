# Verify captures — what `confiture migrate verify` actually emits

These files are **transcripts, not shapes.** Each is the exact `--output` file a
real confiture binary wrote. The tests in `src/lib.rs` and `tests/confiture.rs`
parse them, so the verify verdict rule is proven against the producer rather
than against its changelog. **Do not edit them.** Replacing one means capturing
again from a real binary and updating the tables below.

Hand-written shapes (contradictory, ill-typed or verdict-less payloads) live
inline in the unit tests, next to the rule each one exercises.

## The files

| File | Database | Exit | What it pins |
|---|---|---|---|
| `real-1.12.0-verified.json` | migrated; `001` has a passing `.verify.sql`, `002` has none | 0 | `ok: true`; one `verified` result, one `no_file` |
| `real-1.12.0-failed.json` | migrated; `001`'s sidecar asserts rows that do not exist | 1 | `ok: false`, `failed_count: 1`: a result, not an error |
| `real-1.12.0-no-ledger.json` | empty, **under `--allow-uninitialized`** | 0 | `ok: false`, `was_skipped: true`, `failed_count: 0`: a run that verified nothing (fraiseql/confiture#311) |
| `real-1.12.0-empty-sidecar.json` | migrated; `001`'s sidecar holds only a comment | 0 | status `"skipped"`: neither a failure nor a verification |
| `real-0.44.0-verified.json` | as `real-1.12.0-verified` | 0 | **no `ok` field**; before 1.12.0 the counts are all there is |
| `real-0.44.0-no-ledger.json` | empty, under `--allow-uninitialized` | 0 | no `ok`, `ledger_present: false`, `failed_count: 0` |
| `real-0.20.0-verified.json` | as `real-1.12.0-verified` | 0 | the verify floor: no `ok`, no `ledger_present` |

## Provenance

| | |
|---|---|
| Producers | `fraiseql-confiture` 1.12.0, 0.44.0 and 0.20.0 from PyPI, each run with `uv tool run --from fraiseql-confiture==<version>` |
| Database | PostgreSQL 18.4, one fresh throwaway database per scenario |
| Invocation | `migrate verify --no-config --format json --output <tmp> --migrations-dir <dir>`, with the DSN in `CONFITURE_DATABASE_URL`: the argv the adapter's `plan()` builds. The two `no-ledger` captures add `--allow-uninitialized`, **which the adapter never passes** |
| Captured | 2026-09-19 |

## What the adapter's own argv sees on an empty database

Without `--allow-uninitialized`, no release emits a verify report for a
database that has no migration ledger:

| Release | Exit | Payload |
|---|---|---|
| 1.12.0, 0.44.0 | 2 | `PRECON_1001` error envelope |
| 0.22.0, 0.20.0 | 1 | `INTERNAL_ERROR` envelope (`relation "tb_confiture" does not exist`) |

So the `no-ledger` payloads can't reach the adapter today. The adapter refuses
them anyway, because the day a caller passes the flag, they would.

## A clean exit always leaves a report

0.20.0 (the verify floor), 0.22.0, 0.44.0 and 1.12.0 all write the `--output`
file whenever `migrate verify` exits 0. An exit 0 with no report is not a state
any supported confiture produces, so the adapter treats it as an error.

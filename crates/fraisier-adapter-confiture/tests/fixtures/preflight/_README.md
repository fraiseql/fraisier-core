# Preflight golden fixtures — a cross-repo pact

These files are **product, not test scaffolding.** They are the shared bytes of
the migration risk contract: confiture asserts that it *emits* these shapes,
fraisier asserts that it *parses* them. Changing a fixture changes the contract
in both repositories at once, so a change here needs a matching change in
confiture and a `contract_version` decision.

The contract itself is specified in
[`docs/proposals/migration-risk-contract.md`](../../../../../docs/proposals/migration-risk-contract.md).

## The files

| File | Contract state it pins |
|---|---|
| `v0-no-change-set.json` | A pre-contract payload — no `change_set` at all. The back-compat baseline: an older producer must keep working, classified as "did not classify". |
| `v1-empty.json` | Classified, with `changes: []`. Distinct from the file above, and the distinction is the point: this one means *nothing changes*. |
| `v1-additive.json` | The simplest classified change. |
| `v1-mixed.json` | Three tiers in one set, in migration order — the consumer must preserve order and pick the worst tier independently of it. |
| `v1-unknown-tier.json` | A tier string the consumer does not know. It becomes *unclassified*, never a nearest match, and the valid change beside it survives. |
| `v1-missing-tier.json` | An entry with no `tier` key. Same outcome as above, reached a different way. |
| `v2-future.json` | A `contract_version` from the future. The whole change-set is unusable, and the refusal names both versions. |
| `malformed.json` | `change_set` is a string. A broken envelope invalidates the whole change-set. |
| `v1-real-0.44.0.json` | **A capture, not a shape.** What the real binary emitted for a set covering all five tiers plus one statement it declines to classify. See *Provenance* below. |
| `v1-real-type-change.json` | **A capture.** An `ALTER COLUMN … TYPE`, which arrives unclassified — and therefore denied — on the path this adapter drives. |

## Provenance of the two captures

Every other file here was written by reading the contract: they pin shapes the
producer *must be able to* emit. These two pin what it *does* emit, and the
distinction decides whether you may edit them — **you may not**. They are
transcripts. Correcting one to taste would destroy the only thing it is for.
Replacing one means re-running the capture against a newer binary and saying so
here.

| | |
|---|---|
| Producer | `fraiseql-confiture==0.44.0` from PyPI, installed with `uv tool install` |
| Backend | **regex**, not AST — `pglast` is not a runtime dependency of the package, so a standard install classifies with the regex backend while `--version` reports the same string either way |
| Invocation | `migrate preflight --no-config --format json --output <tmp> --migrations-dir <dir>` — the argv the adapter's own `plan()` builds, with no `--against` |
| DSN | `CONFITURE_DATABASE_URL`, deliberately unreachable: the static classification path never connects, so the capture reproduces on a machine with no PostgreSQL |
| Corpus | `v1-real-0.44.0`: one migration per tier (`CREATE TABLE`, `RENAME COLUMN`, `CREATE INDEX`, `TRUNCATE`, `DROP TABLE`) plus a `DO $$ … $$` block. `v1-real-type-change`: one `ALTER COLUMN … TYPE`. Each migration has a `down` file, so both results are error-free lints |

**Why the type change is unclassified**, and why that is not a defect to fix
here: telling a widening `varchar(10) → varchar(20)` from a narrowing one needs
the column's current type and the server version. Confiture gathers those only
when it is asked to compare against a live database, and this adapter does not
ask — so it declines to guess rather than guessing wrong. The consequence is
operator-visible: with a `[policy]` section configured, any deploy containing a
type change is denied. That is the contract working, not failing.

Two properties of these files are load-bearing and easy to erase by tidying:

- An unclassified entry carries **no `tier` key at all** — not `"tier": null`.
  Absence is how the producer says *I decline to classify this*, and the consumer
  must reach `unclassified` by that route.
- `window_safe` is **`false`** in both — one drops a table, the other changes a
  column type. They are the only fixtures here that are not window-safe, which is
  why the adapter's fixture table pins the verdict per row rather than once for
  all of them.

The AST backend was run over a separate nine-statement corpus at capture time and
produced identical tiers. That is an observation recorded here, not a gate: a
differential across two confiture installs is not something a Rust test suite can
run, and its regression home is confiture.

## Which cases live here, and which do not

A fixture belongs here when it pins a shape the **producer** must be able to
emit, or a shape it must never emit and the consumer must survive
(`malformed.json`). Consumer-only robustness cases — a change entry that is an
integer, a `changes` key that is an object — are exercised inline in the adapter
tests instead. Asking confiture to produce a corrupted entry as a contract
obligation would be nonsense; those cases test fraisier's parser, not the pact.

## Scenario

The **hand-authored** fixtures share one coherent migration set so they read as a
story rather than as unrelated blobs (`v1-real-0.44.0.json` stands apart: its
corpus is whatever the capture was run against, and it is not editable to fit):

- `20260804120000` — add `tb_user.nickname` (additive)
- `20260804120050` — index `tb_order.placed_at` (lock-risky)
- `20260804120100` — drop `tb_user.legacy_flag` (irreversible)

`migration` is the **version prefix**, not the filename — matching
`issues[].migration`, which already carries the version.

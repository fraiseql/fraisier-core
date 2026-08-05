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

## Which cases live here, and which do not

A fixture belongs here when it pins a shape the **producer** must be able to
emit, or a shape it must never emit and the consumer must survive
(`malformed.json`). Consumer-only robustness cases — a change entry that is an
integer, a `changes` key that is an object — are exercised inline in the adapter
tests instead. Asking confiture to produce a corrupted entry as a contract
obligation would be nonsense; those cases test fraisier's parser, not the pact.

## Scenario

The fixtures share one coherent migration set so they read as a story rather
than as unrelated blobs:

- `20260804120000` — add `tb_user.nickname` (additive)
- `20260804120050` — index `tb_order.placed_at` (lock-risky)
- `20260804120100` — drop `tb_user.legacy_flag` (irreversible)

`migration` is the **version prefix**, not the filename — matching
`issues[].migration`, which already carries the version.

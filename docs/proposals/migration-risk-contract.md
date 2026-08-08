# Migration risk contract — the change-set that crosses the adapter seam

**Status:** RATIFIED — the shape below is the cross-repo contract. It is
implemented by `fraisier-adapter-confiture` (consumer) and by confiture's
`migrate preflight --format json` (producer).

**Date:** 2026-08-05
**Contract version:** 1
**Scope:** the `MigrationAdapter` preflight surface — `PreflightReport` and the
`change_set` it carries. Paired with [fraiseql/confiture#197][c197] and
[fraisier-core#44][i44], under epic [fraiseql/fraiseql#963][e963].

[c197]: https://github.com/fraiseql/confiture/issues/197
[i44]: https://github.com/fraiseql/fraisier-core/issues/44
[e963]: https://github.com/fraiseql/fraiseql/issues/963
[i964]: https://github.com/fraiseql/fraiseql/issues/964
[c154]: https://github.com/fraiseql/confiture/issues/154

---

## 1. Why the tier needs its own typed home

Before this contract, the entire risk vocabulary crossing the migration-adapter
seam was one tri-state boolean: `PreflightReport.window_safe: Option<bool>`. That
field answers a specific question — *can version N-1 and version N share this
database for a blue-green hold window?* — and deliberately answers nothing else.

A per-change risk tier squeezed through that surface would have to be recovered
by string-matching `issues[].code`, which the window-safety gate explicitly
forbids: *"no fallback to pattern-matching issue codes — the typed verdict is the
contract."* Inference from codes is how a producer-side rename becomes a
silent consumer-side misclassification. So the tier travels as typed data or it
does not travel.

## 2. Language boundary and versioning

The contract crosses a **Python producer → Rust consumer** boundary as JSON over
the confiture CLI. That placement is deliberate and recorded in [#964][i964]:
the seam is already JSON (`window_safe` has crossed it in production since
[confiture#154][c154]), while confiture's Rust core is at 0.3.9 against a 0.38.1
Python CLI. The contract is therefore designed **once**, versioned explicitly,
and a future Rust implementation reimplements the same versioned shape — a
serializer rewrite, not a redesign.

Versioning uses a **payload-level `contract_version`**, not the IPC protocol
version:

- `contract_version: u32`, a major, starting at `1`. It lives **inside** the
  `change_set` object.
- Adding a field to a change entry does **not** bump it. Consumers ignore keys
  they do not recognise.
- Removing a field, renaming a field, or changing what a tier *means* bumps it.
- `fraisier-ipc`'s `PROTOCOL_VERSION` is **not** bumped for this. A protocol bump
  invalidates every external IPC adapter for what is a purely additive payload
  field; capability strings, not protocol majors, are this project's
  feature-negotiation idiom.

### Forward compatibility fails safe, and loudly

A consumer that reads a `contract_version` **greater** than the version it
understands treats the change-set as **absent** and names both versions in the
refusal reason. It does not attempt a best-effort parse. A payload written to a
contract we cannot read is not a payload we may approve on the operator's behalf.

## 3. Capability negotiation

One capability string, advertised in `AdapterDescription::capabilities`
alongside the existing `preflight` and `window_safe`:

| Capability | Means |
|---|---|
| `risk_tier` | This adapter emits a change-set with per-change risk tiers. |

One string, not two. A change-set without tiers gives the gate nothing to decide
on, and tiers without a change-set have nothing to attach to; there is no useful
intermediate state to advertise.

An adapter must advertise `risk_tier` only when the **installed** producer can
actually emit a change-set — for the confiture adapter that means gating the
capability on the detected confiture version, not hard-coding it in a static
list. Advertising a capability the installed binary cannot fulfil converts every
deploy into a denial: safe, but useless. Not advertising it is the honest signal
*"I do not classify"*, which callers handle deliberately (§6).

## 4. Wire shape

Added to the existing `migrate preflight --format json` payload. Everything
outside `change_set` is unchanged.

```json
{
  "ok": true,
  "window_safe": true,
  "summary": { "errors": 0, "warnings": 1, "info": 0, "migrations_checked": 3 },
  "issues": [],
  "change_set": {
    "contract_version": 1,
    "changes": [
      {
        "kind": "add_column",
        "object": "public.tb_user.nickname",
        "migration": "20260804120000",
        "tier": "additive",
        "detail": "ADD COLUMN nickname text NULL"
      },
      {
        "kind": "drop_column",
        "object": "public.tb_user.legacy_flag",
        "migration": "20260804120100",
        "tier": "irreversible",
        "detail": "DROP COLUMN legacy_flag"
      }
    ]
  }
}
```

### `change_set` is an object, never a bare array

This is load-bearing. The object wrapper is what lets the two states be
distinguished:

| State | Wire | Means |
|---|---|---|
| classified, nothing to do | `"change_set": {"contract_version": 1, "changes": []}` | The adapter looked. There is nothing to change. **Safe.** |
| not classified | `change_set` key absent | Nobody looked. **Unknown, and unknown is not safe.** |

A bare `changes: []` array conflates the two, and the conflation resolves in the
dangerous direction: an unclassified migration would present as an empty plan and
auto-apply. This is the same presence distinction `window_safe: Option<bool>`
already encodes, carried one level deeper.

### Field rules

| Field | Type | Required | Rule |
|---|---|---|---|
| `contract_version` | integer | yes | Absent or non-integer ⇒ the whole change-set is unusable (`None`). |
| `changes` | array | no | Absent is equivalent to `[]` — the producer classified and found nothing. |
| `changes[].kind` | string | yes | Stable machine code, `snake_case`, e.g. `add_column`. Rendered verbatim; never parsed for meaning by the consumer. |
| `changes[].object` | string | yes | Fully-qualified target: `schema.table`, `schema.table.column`, `schema.index`. Rendered to the operator, so it must identify the object unambiguously without further lookup. |
| `changes[].migration` | string \| null | no | **The migration version prefix** (`"20260804120100"`), *not* the full filename. This matches `issues[].migration`, which already carries the version, and keeps the dry-run render's column width bounded. |
| `changes[].tier` | string \| null | no | One of §5. Absent, null, or unrecognised ⇒ unclassified (§6). |
| `changes[].detail` | string \| null | no | One human-readable line for the plan render. Never parsed. Must not contain a DSN or any credential. |

## 5. Tier taxonomy

Five tiers, `snake_case` on the wire.

| Tier | Meaning | Canonical examples |
|---|---|---|
| `additive` | Adds a new object. No existing reader or writer can break. | `CREATE TABLE`, `ADD COLUMN … NULL`, `CREATE INDEX CONCURRENTLY` |
| `reversible` | Changes existing state, with a proven `down` path that restores it. | `ALTER … SET DEFAULT`, widening `varchar(n)` |
| `lock_risky` | Semantically safe, but takes a lock that can stall a hot table. | `ADD COLUMN … NOT NULL DEFAULT` on older PG, non-concurrent `CREATE INDEX`, table rewrite |
| `destructive` | Destroys data or an object, but the loss is bounded and recoverable from backup. | `DELETE`, `TRUNCATE`, `DROP INDEX` |
| `irreversible` | Destroys data with no `down` path that can restore it. | `DROP COLUMN`, `DROP TABLE`, narrowing a type |

### Boundary rulings

Stated here so they are not re-litigated per pull request:

- **`DROP INDEX` is `destructive`, not `irreversible`.** The index is rebuildable
  from the data it indexes. The cost is time and load, not information.
- **`DROP COLUMN` is `irreversible` even when a `down.sql` exists.** The down path
  restores the *schema*; it cannot restore the *data*. Reversibility here means
  the state is recoverable, not that a script exists.
- **A change qualifying for two tiers takes the more severe one.** A
  non-concurrent `CREATE INDEX` on a table being rewritten is `lock_risky`; a
  `DROP COLUMN` that also takes a long lock is `irreversible`.
- **A tier the consumer does not recognise is unclassified**, never a
  nearest match. A future `"quantum"` tier parses to "no tier", which denies;
  it never rounds down to `destructive` because the strings look similar.

### What the ordering is, and is not

The tiers carry a total order, least to most severe:

```
additive < reversible < lock_risky < destructive < irreversible
```

That order exists for exactly two purposes: computing the *worst* tier in a
change-set (for the approval request and the plan header), and sorting the
dry-run render worst-first. **It is not how policy decisions are made.** Policy
maps each tier to an action independently, so an operator who considers a
`lock_risky` index build more dangerous than a `DROP INDEX` on their workload
expresses that in configuration, not by arguing about this ordering.

## 6. Absence is never safety

Four distinct ways the risk answer can be missing, and what each means. None of
them mean "proceed":

| Situation | Consumer sees | Meaning |
|---|---|---|
| Adapter does not advertise `risk_tier` | capability absent | The producer cannot classify at all — e.g. a confiture older than the release that implements this contract. |
| Capability advertised, `change_set` key absent | `None` | A producer bug. It claimed it classifies and then did not. Say so in the reason. |
| `contract_version` too new | change-set present but unusable | We cannot read this payload; naming both versions is the actionable part. |
| A change entry with no recognised `tier` | `tier: None` on that entry | One change is unclassified. The others remain classified. |

Every one of these resolves to *unclassified*, and unclassified is denied by
default. This mirrors the rule already governing the two adjacent contracts: a
`MethodNotSupported` is never a passing report, and a `window_safe` of `None` is
a refusal, not a pass.

### One malformed entry must not become a hole

A change entry the consumer cannot parse is replaced by an **unclassified
placeholder**, not dropped. Dropping it would shrink the set silently, and a
shorter list of fully-classified changes reads as a *cleaner* plan than the truth
— the one failure direction this contract exists to prevent. A broken *envelope*,
by contrast, invalidates the whole change-set: if the wrapper is untrustworthy
the entries inside it cannot be trusted either.

## 7. Rust types

```rust
/// The contract revision this build understands.
pub const RISK_CONTRACT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RiskTier {
    Additive,
    Reversible,
    LockRisky,
    Destructive,
    Irreversible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SchemaChange {
    pub kind: String,
    pub object: String,
    pub migration: Option<String>,
    /// `None` ⇒ unclassified ⇒ denied. Never inferred from `kind`.
    pub tier: Option<RiskTier>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChangeSet {
    pub contract_version: u32,
    pub changes: Vec<SchemaChange>,
}
```

`PreflightReport` gains one optional field and becomes `#[non_exhaustive]`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub change_set: Option<ChangeSet>,
```

`skip_serializing_if` keeps the serialized form byte-identical for adapters that
do not classify, so no downstream consumer sees a new `null` appear.

The version check is centralised in one accessor so no call site can forget it:

```rust
impl PreflightReport {
    pub fn usable_change_set(&self) -> Result<&ChangeSet, ChangeSetUnavailable>;
}

pub enum ChangeSetUnavailable {
    NotEmitted,
    VersionTooNew { found: u32, understood: u32 },
}
```

## 8. Golden fixtures — the cross-repo pact

`crates/fraisier-adapter-confiture/tests/fixtures/preflight/` holds one file per
contract state. Both repositories test against the same bytes: confiture asserts
it *emits* these shapes, fraisier asserts it *parses* them.

| File | State |
|---|---|
| `v0-no-change-set.json` | A pre-contract payload. No `change_set`. Back-compat baseline. |
| `v1-empty.json` | Classified, `changes: []`. Nothing to change. |
| `v1-additive.json` | One `additive` change. |
| `v1-mixed.json` | `additive` + `lock_risky` + `irreversible`. |
| `v1-unknown-tier.json` | An unrecognised tier string beside a valid change. |
| `v1-missing-tier.json` | An entry with no `tier` key beside a valid change. |
| `v2-future.json` | `contract_version: 2`. Must be treated as absent. |
| `malformed.json` | `change_set` is a string. Consumer robustness. |

## 9. Trust model

fraisier **consumes** the adapter's classification; it does not re-derive it. An
adapter that reports every change as `additive` is believed. That is the trust
boundary by construction — the same one that already applies to `window_safe` —
and the mitigation is that the migration adapter is operator-chosen
configuration, at the same trust level as any other command in `fraisier.toml`.
The contract's job is to make sure that *silence, error, and confusion* are never
mistaken for a clean bill of health; it is not to defend against a producer that
lies.

One consequence of that boundary is worth stating plainly, because it is where
the obligation on `changes[].detail` earns its keep: **`detail` reaches the
operator's terminal verbatim.** The plan render appends it to the row unescaped,
so a producer that put a DSN in it has printed and logged that DSN. This is
acceptable at exactly the trust level above — the producer is a binary the
operator installed, reading migrations the operator wrote — and it is *not*
acceptable for a payload that has left the contract. That is why the adapter's
`unclassified_placeholder` and `warn_unusable` name the entry's **shape** and
**position** and quote nothing from inside it: a payload that broke the envelope
also broke its promise about `detail`.

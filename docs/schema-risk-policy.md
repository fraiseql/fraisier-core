# Schema risk policy gate (recipe)

Block a deploy whose pending migration does something destructive, and let a
human — or an agent with authority — sign it off. The gate reads the **risk
tier** the migration adapter assigns to each planned schema change, applies the
policy you configured, and refuses anything it cannot classify.

```toml
[policy]
# Tiers that apply without asking anyone.
auto_apply       = ["additive", "reversible"]
# Tiers that need the approval hook to say yes.
require_approval = ["lock_risky", "destructive", "irreversible"]
# What to do with a change nobody classified. "deny" (default) or
# "require_approval" — auto-applying the unclassified is not expressible.
unclassified     = "deny"
# The hook. Without one, anything needing sign-off is refused, never passed.
approval_command      = "scripts/deploy/approve.sh"
approval_timeout_secs = 300
```

The section's **presence** is the opt-in. With no `[policy]` section the tier
gate does not run at all and every deploy behaves exactly as it did before.

## What it blocks

Each pending change carries one tier:

| Tier | Means | Examples |
|---|---|---|
| `additive` | Adds an object; no existing reader or writer can break | `CREATE TABLE`, `ADD COLUMN … NULL`, `CREATE INDEX CONCURRENTLY` |
| `reversible` | Changes existing state, with a `down` path that restores it | `ALTER … SET DEFAULT`, widening a `varchar(n)` |
| `lock_risky` | Semantically safe, but takes a lock that can stall a hot table | non-concurrent `CREATE INDEX`, a table rewrite |
| `destructive` | Destroys data, recoverably from backup | `DELETE`, `TRUNCATE`, `DROP INDEX` |
| `irreversible` | Destroys data with no `down` path that restores it | `DROP TABLE`, narrowing a type, `DROP COLUMN` |

A change-set takes the **worst** action any single change maps to: `deny` beats
`require_approval` beats `auto_apply`. The refusal names the objects
responsible, and the full list reaches the approval hook.

A tier listed in **neither** list is denied. That is deliberate: a tier added to
the taxonomy in a later release must not silently auto-apply on a config written
today.

### Absence is never safety

Every way of not knowing is a refusal, and the refusal says which one it was:

| Situation | Result |
|---|---|
| Preflight did not run (`preflight_mode = "off"`, `--skip-preflight`) | refused — nothing inspected the changes |
| The migration adapter could not be built (an unset DSN env var) | refused — same reason, one step earlier |
| The adapter does not advertise `risk_tier` | refused (or sent to the hook, with `unclassified = "require_approval"`) |
| It advertises `risk_tier` and emits no change-set | refused — that is an adapter bug, and the message says so |
| The change-set is stamped with a contract version this build cannot read | refused, naming both versions. Never approvable: an approver would be signing off on a payload nobody can read |
| One entry carries a tier this build does not recognise | refused — an unknown tier is *no* tier, never a nearest match |
| One entry carries **no tier**, because the producer declined to classify it | refused — and the reason names the object, so you can find the statement |

The last row is the one you are most likely to meet, and with the confiture
adapter it has a specific and common cause: **an `ALTER COLUMN … TYPE` is
unclassified.** Telling a widening `varchar(10) → varchar(20)` from a narrowing
one needs the column's current type and the server version; confiture gathers
those only when it is asked to compare against a live database, and fraisier does
not ask. So it declines to guess rather than guessing wrong, and a configured
`[policy]` refuses the deploy. That is the contract working as designed — see
*How to unblock a refused deploy* below.

Configuring `[policy]` together with `[migration].preflight_mode = "off"`
therefore refuses **every** deploy. `fraisier validate-config` warns about that
combination rather than leaving you to discover it at the gate.

## Which deploys are gated

| Path | Tier policy | Window-safety rule |
|---|---|---|
| single-host | opt-in via `[policy]` | — |
| multi-host rolling | opt-in via `[policy]` | — |
| blue-green | opt-in via `[policy]` | **always on** |
| `fraisier rollback` | not gated | — |

Blue-green additionally refuses any migration confiture cannot certify as
forward-compatible for a two-version window, with or without a `[policy]`
section — N-1 and N share one database for the hold window, and that rule is not
something a config can switch off. It is the same rule, the same verdict and the
same block as before this gate existed; only the message is new.

`fraisier rollback` is **not** gated. It migrates *down*, while the preflight
report describes the pending *forward* changes — judging one by the other would
rule on a change-set the run will not apply, and would put an approver between
an operator and an emergency rollback.

## The approval hook

`approval_command` runs via `sh -c` when — and only when — a change lands in
`require_approval`. It receives:

- the request as **JSON on stdin**:

  ```json
  {
    "fraise": "checkout",
    "environment": "production",
    "worst_tier": "irreversible",
    "reasons": [
      {
        "tier": "irreversible",
        "object": "public.tb_user.legacy_flag",
        "kind": "drop_column",
        "migration": "20260804120100_drop_legacy"
      }
    ]
  }
  ```

- and the deploy's identity in the environment: `FRAISIER_APPROVAL_FRAISE`,
  `FRAISIER_APPROVAL_ENVIRONMENT`, `FRAISIER_APPROVAL_WORST_TIER`,
  `FRAISIER_APPROVAL_CHANGE_COUNT`.

**Exit 0 approves.** The first non-empty line of stdout is recorded as the
approver; with no output, the command itself is recorded.

Everything else refuses — a non-zero exit (quoting the hook's first stderr
line), a command that cannot be executed, a spawn failure, or a hook still
running at `approval_timeout_secs`. There is no `--force` and no `--approve`
flag: approval arrives only through the hook, so "an agent with authority" means
configuring a hook that can authenticate that agent.

```sh
#!/usr/bin/env bash
# scripts/deploy/approve.sh — page whoever is on call, and answer for them.
set -euo pipefail
request=$(cat)                       # the JSON payload, on stdin
if pager-ack --title "schema change on $FRAISIER_APPROVAL_FRAISE" \
             --detail "$request" --timeout 240; then
  echo "$(pager-ack --last-responder)"   # who approved → the audit record
  exit 0
fi
echo "no acknowledgement within 4 minutes" >&2
exit 1
```

## Previewing a change

`fraisier deploy --dry-run` shows the classified change-set and the verdict the
gate **would** reach — terraform-plan for schema change. It runs the same
`preflight` and the same decision function the deploy does, and it never calls
the approval hook: a dry-run reports that sign-off would be needed, it does not
go and ask for it.

```
deploy plan for checkout/production → host web-1
  artifact:  release
  migration: confiture (in-process)
  service:   systemd
  health:    http
  database_url_env: CHECKOUT_DATABASE_URL
  settings:  migrations_path

  schema change-set (confiture 0.40.0, contract v1) — 3 changes:
    [irreversible]  drop_column   public.tb_user.legacy_flag     20260804120100
    [lock_risky]    create_index  public.tb_order.idx_placed_at  20260804120050
    [additive]      add_column    public.tb_user.nickname        20260804120000

  policy: WOULD BLOCK — 2 change(s) require approval (worst tier: irreversible)
    · create_index public.tb_order.idx_placed_at (20260804120050)
    · drop_column public.tb_user.legacy_flag (20260804120100)
    approval hook: scripts/deploy/approve.sh

(dry run — nothing was executed)
```

Changes are listed **worst-first** — unclassified, then most severe down to
least — because an operator scans the top of a list. Within one tier they keep
the order they will apply in.

A blue-green plan adds the window-safety verdict, which its baseline reads from
the same report:

```
  window safety: certified for the two-version blue-green window
```

### It reads the database now

A change-set can only come from `MigrationAdapter::preflight`, which for
confiture means running `confiture migrate preflight` against the target
database. **A dry-run therefore needs a DSN where it previously needed
nothing.** If you run `deploy --dry-run` in CI as a cheap config check, either
give that job database credentials or add `--skip-preflight`.

`--dry-run --skip-preflight` prints the plan exactly as it did before this
feature existed — no change-set, no verdict, no new lines.

### When it cannot see

A plan that cannot reach the schema still prints, still exits 0, and never reads
as clean:

```
  schema change-set: UNAVAILABLE — confiture 0.38.1 does not advertise
    `risk_tier`, so no pending schema change was classified.
    Risk is unknown, not zero.
```

Every such line ends on that phrase — **Risk is unknown, not zero.** — because a
plan is read fast, and a missing change-set at a glance looks exactly like an
empty one.

The machine form keeps the two apart without parsing that prose — which is the
whole point, because *"nothing to change"* and *"nobody looked"* are one careless
`if` apart:

| State | `change_set` | `change_set_unavailable` |
|---|---|---|
| The adapter looked, nothing changes | `{ "changes": [], … }` | `null` |
| Nobody looked, or nobody could classify | `null` | `{ "code": …, "detail": … }` |

The codes are stable: `preflight_skipped`, `preflight_off`,
`adapter_unavailable`, `no_preflight_capability`, `preflight_failed`,
`no_risk_tier_capability`, `no_change_set`, `unreadable_change_set`.

Degradation reasons are stripped of URL credentials before they are printed or
logged — a failed connection names the host, never the password.

### Gating a pipeline on the plan

`--dry-run` exits 0 whenever a plan was produced, including one that reports a
block: the plan is the deliverable. Add `--fail-on-block` to exit nonzero when
the deploy it previews would not go through — both a refusal and a decision
waiting on a human, because neither one deploys unattended.

```sh
fraisier deploy --dry-run --fail-on-block --json   # the plan, and a CI gate
```

With no `[policy]` section there is no verdict, so `--fail-on-block` has nothing
to gate on and never fires.

## How to unblock a refused deploy

1. **Read the reason.** It names the rule that fired and the objects
   responsible; a blue-green window refusal names the window instead.
2. If the change genuinely needs to happen: **configure a hook** and approve it,
   or move the tier to `auto_apply` if your project considers it routine.
3. If the change was not intended: fix the migration and re-deploy. Splitting a
   destructive migration into expand → deploy → contract is usually the answer,
   and is what makes the change `additive` twice over.
4. If the adapter did not classify: **upgrade confiture to ≥ 0.44.0.** That is
   the fix. fraisier withholds `risk_tier` from every earlier release — the
   change-set arrived in 0.43.0, but that release still misreads `DROP TABLE`
   ([confiture#206](https://github.com/fraiseql/confiture/issues/206)), and a
   classifier that cannot be trusted about the window is not trusted about the
   tier either. Below the floor a `[policy]` section classifies nothing and
   refuses everything; `fraisier doctor` reports the installed version against
   the floor.

   For a producer that will **never** classify — an external adapter that lints
   but does not tier — `unclassified = "require_approval"` routes its deploys
   through the hook instead. That is the option for a producer with no upgrade
   path, not the general answer to a refusal.

5. If the refused change is an `ALTER COLUMN … TYPE`, nothing is broken.
   Confiture will not guess whether a type change widens or narrows without the
   column's current type, and it gathers that only when comparing against a live
   database — which fraisier does not ask it to do. The change arrives
   unclassified and is refused. Approve it through the hook, or split it into
   expand → backfill → contract, which classifies cleanly at every step.

## Audit

Every decision is recorded:

- **Traces/logs** — `schema policy: allowed | approved | refused`, with
  `deploy.fraise` and `deploy.environment` attributes, and the approver on an
  approval.
- **The failure sink** — a blocked deploy fires `[schedule].notify` with the
  event `policy-blocked` (rather than `scheduled-deploy-failed`), so an
  unattended deploy held for sign-off is distinguishable at a glance from one
  that broke.
- **The saga** — a refusal is a `preflight` step failure, so it lands in the
  deploy's persisted state and its exit code, like every other blocked deploy.

## Security

The hook receives the deploy's identity and the triggering changes — never the
adapter context, so no DSN, secret name, or adapter setting can reach it. It
does inherit the fraisier process environment, exactly as `[schedule].notify`,
`[[checks]]` and the `command` adapters do; that is the operator's own
environment, and the same trust boundary as the rest of your `fraisier.toml`.

The request goes on **stdin**, never argv: an argv value is world-readable in
process listings (`ps`) to every user on the host, and a payload that starts
with `-` can be mistaken for a flag.

A refusal quotes the hook's first stderr line (bounded, into the saga reason and
the notify webhook). Keep secrets out of the hook's stderr, since the notify sink
may be a broader-audience channel than your local logs.

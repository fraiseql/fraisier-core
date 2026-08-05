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
| The adapter does not advertise `risk_tier` | refused (or sent to the hook, with `unclassified = "require_approval"`) |
| It advertises `risk_tier` and emits no change-set | refused — that is an adapter bug, and the message says so |
| The change-set is stamped with a contract version this build cannot read | refused, naming both versions. Never approvable: an approver would be signing off on a payload nobody can read |
| One entry carries a tier this build does not recognise | refused — an unknown tier is *no* tier, never a nearest match |

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

## How to unblock a refused deploy

1. **Read the reason.** It names the rule that fired and the objects
   responsible; a blue-green window refusal names the window instead.
2. If the change genuinely needs to happen: **configure a hook** and approve it,
   or move the tier to `auto_apply` if your project considers it routine.
3. If the change was not intended: fix the migration and re-deploy. Splitting a
   destructive migration into expand → deploy → contract is usually the answer,
   and is what makes the change `additive` twice over.
4. If the adapter cannot classify: upgrade the producer (confiture ≥ 0.40.0
   emits the change-set) — or set `unclassified = "require_approval"` to route
   its deploys through the hook instead of refusing them outright.

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

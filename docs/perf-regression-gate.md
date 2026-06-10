# Perf-regression rollback gate (recipe)

Roll a deploy back automatically when a new release makes a database mutation
slower. This rides FraiseQL's `fraiseql perf regression-scan` (v2.6.0+, the
"perf-observability seam", FraiseQL #392): with `--fail-on-regression` it exits
non-zero when any `(object_type, modification_type)` p50 latency regressed past
its thresholds, and exits 0 otherwise. Operational errors (database unreachable,
bad DSN) always exit non-zero, so "a regression appeared" is distinguishable from
"the scan could not run."

## Recommended: the `command` health adapter

The first-class gate is `[health].adapter = "command"`: a health adapter that runs
the perf scan as the saga's **`Health`** step. It works for **any** project —
`confiture` included — and carries the scan's output into the rollback reason and
the `[schedule].notify` failure webhook.

```toml
[migration]
adapter          = "confiture"   # or any adapter — the gate is migration-agnostic
# The scan reads $DATABASE_URL. The health command inherits this same mapping, so
# the DSN travels by environment, never on argv (see "Security" below).
database_url_env = "DATABASE_URL"

[health]
adapter    = "command"
command    = "fraiseql perf regression-scan --fail-on-regression --json"
timeout_ms = 60000   # optional; fail-closed probe timeout (default 60000)
```

Requires **fraiseql v2.6.0+** (the perf-observability seam, FraiseQL #392).

The gate passes iff the command exits 0. On a regression the scan exits 1, the
deploy ends in `RolledBack { failed_step = "health" }`, and the saga restores the
previously-active release and reverts the migration. With `--json`, the adapter
parses the scan's report and names the top regressed operation in the rollback
reason (and the failure webhook payload when `[schedule].notify` is configured) —
e.g. `perf regression: order/UPDATE p50 +42% (12ms→17ms), 3 more` — so the alert
says *what* regressed, not just "health check failed." Drop `--json` and the gate
still works, but the detail degrades to the scan's plain output excerpt.

Fail-closed by construction: a spawn failure (missing `fraiseql` binary) or a
timeout is an *operational* error, distinct from "unhealthy" — both still roll the
deploy back rather than committing a release as healthy. The DSN is inherited from
`[migration].database_url_env`; no separate `[health]` DSN field is needed.

## Fallback: the command-migration `verify` hook

If your project drives migrations through the **`command`** migration adapter
(`[migration].adapter = "command"`), its post-migration `verify` command also
gates the deploy: the saga's `Verify` step runs it, and a non-zero exit rolls the
release back. Prefer the health adapter above; reach for this only if you are
already on the `command` migration adapter and want the gate inline with verify.

```toml
[migration]
adapter          = "command"
# The scan reads $DATABASE_URL. Map your source env var to it here; the DSN then
# travels by environment, never on argv (see "Security" below).
database_url_env = "DATABASE_URL"

[migration.settings.commands]
# your project's normal migration commands:
current_revision = "mytool current"
up               = "mytool migrate"
down_to          = "mytool rollback --to $FRAISIER_TARGET"
# the perf gate — runs after migrate + activate; a regression rolls the deploy back:
verify           = "fraiseql perf regression-scan --fail-on-regression"
```

On a regression the deploy ends in `RolledBack { failed_step = "verify" }`, and
the saga restores the previously-active release.

Two reasons this is the fallback, not the recommendation:

1. **`command` migrations only.** A project using `[migration].adapter =
   "confiture"` (the FraiseQL default) cannot use this recipe — there is one
   migration adapter per deploy, and `confiture` owns `verify`. The health adapter
   has no such restriction.
2. **The rollback reason does not name the regression.** The `Verify` step reports
   a generic `post-migration verify failed N check(s)`; the scan's per-operation
   detail is discarded. The health adapter carries the detail through instead.

## Security

Pass the DSN by **environment**, never on argv. Do **not** write
`fraiseql perf regression-scan --database $DATABASE_URL`: the shell interpolates
the DSN into the process arguments, where it is visible in process listings
(`ps`) to every user on the host. The `[migration].database_url_env` mapping above
exports the DSN to the scan's environment instead, keeping it out of argv and logs.

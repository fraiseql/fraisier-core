# Perf-regression rollback gate (recipe)

Roll a deploy back automatically when a new release makes a database mutation
slower. This rides FraiseQL's `fraiseql perf regression-scan` (v2.6.0+, the
"perf-observability seam", FraiseQL #392): with `--fail-on-regression` it exits
non-zero when any `(object_type, modification_type)` p50 latency regressed past
its thresholds, and exits 0 otherwise. Operational errors (database unreachable,
bad DSN) always exit non-zero, so "a regression appeared" is distinguishable from
"the scan could not run."

## Works today: the command-migration `verify` hook

If your project drives migrations through the **`command`** migration adapter
(`[migration].adapter = "command"`), its post-migration `verify` command already
gates the deploy: the saga's `Verify` step runs it, and a non-zero exit rolls the
release back — re-activating the prior artifact and reverting the migration.

Point `verify` at the perf scan:

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

## Limitations (why this is a fallback, not the feature)

1. **`command` migrations only.** A project using `[migration].adapter =
   "confiture"` (the FraiseQL default) cannot use this recipe — there is one
   migration adapter per deploy, and `confiture` owns `verify`.
2. **The rollback reason does not name the regression.** The `Verify` step reports
   a generic `post-migration verify failed N check(s)`; the scan's per-operation
   detail (which `(object_type, modification_type)` regressed, and by how much) is
   not carried into the rollback reason or the failure notification.

A first-class, adapter-agnostic perf health gate — `[health].adapter = "command"`
— that works for **any** project (confiture included) and names the regressed
operations in the rollback reason and the `[schedule].notify` webhook is tracked
in GitHub issue #11.

## Security

Pass the DSN by **environment**, never on argv. Do **not** write
`fraiseql perf regression-scan --database $DATABASE_URL`: the shell interpolates
the DSN into the process arguments, where it is visible in process listings
(`ps`) to every user on the host. The `[migration].database_url_env` mapping above
exports the DSN to the scan's environment instead, keeping it out of argv and logs.

#!/usr/bin/env bash
# perf-scan-stub.sh — a dependency-free stand-in for
# `fraiseql perf regression-scan` that reproduces the fraiseql v2.6.0
# "perf-observability seam" (FraiseQL #392), so fraisier's command health adapter
# and its rollback e2e can be developed and CI-tested without a live database or a
# released `fraiseql`.
#
# Contract pinned from fraiseql v2.6.0 source
# (crates/fraiseql-cli/src/commands/perf/analysis.rs:55-117, mod.rs:84-85):
#   --json                Print the {findings, skipped, summary} report on stdout.
#   --fail-on-regression  Exit 1 iff at least one regression was found; without the
#                         flag a regression is still reported but the exit stays 0
#                         (it is a report, not a gate). Other args are ignored, so
#                         the real invocation `... regression-scan --fail-on-regression
#                         --json` drives the stub unchanged.
# Operational errors always exit non-zero, before any report is printed.
#
# Deterministic knobs (env), so a test can drive every branch:
#   REGRESS=1   Simulate one regressed operation (order/UPDATE, p50 12ms→17ms).
#   FAIL_OP=1   Simulate an operational error (e.g. database unreachable): a
#               non-zero exit, a stderr diagnostic, and NO JSON on stdout.
set -euo pipefail

fail_on_regression=0
json=0
for arg in "$@"; do
	case "$arg" in
	--fail-on-regression) fail_on_regression=1 ;;
	--json) json=1 ;;
	*) ;; # ignore subcommand words and any other flags (DSN travels by env)
	esac
done

# Operational error: non-zero exit before printing any report, mirroring an `Err`
# returned ahead of the renderer. Distinct from the regression gate's exit 1.
if [ "${FAIL_OP:-0}" = "1" ]; then
	echo "perf-scan-stub: operational error (simulated: database unreachable)" >&2
	exit 2
fi

if [ "${REGRESS:-0}" = "1" ]; then
	findings='[
    {
      "object_type": "order",
      "modification_type": "UPDATE",
      "baseline_p50": 12.0,
      "baseline_p95": 20.0,
      "recent_p50": 17.0,
      "recent_p95": 28.0,
      "pct_change": 41.67,
      "baseline_samples": 120,
      "recent_samples": 140
    }
  ]'
	regressions=1
else
	findings='[]'
	regressions=0
fi

if [ "$json" = "1" ]; then
	cat <<JSON
{
  "findings": $findings,
  "skipped": [],
  "summary": {
    "groups_analyzed": 2,
    "regressions": $regressions,
    "total_samples": 200,
    "excluded_samples": 0
  }
}
JSON
else
	# Human-format, greppable output (mirrors the real WARN / summary lines).
	if [ "$regressions" -gt 0 ]; then
		echo "WARN  order/UPDATE  p50 +42% (12ms->17ms)"
	fi
	echo "scanned 2 groups, ${regressions} regression(s), 200 samples, 0 excluded"
fi

if [ "$fail_on_regression" = "1" ] && [ "$regressions" -gt 0 ]; then
	exit 1
fi
exit 0

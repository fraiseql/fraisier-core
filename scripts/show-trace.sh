#!/usr/bin/env bash
#
# Render fraisier deploy traces from a Jaeger instance as text — headless trace
# inspection, no browser. Reads Jaeger's HTTP API and prints each deploy as a
# span tree (the `saga.deploy` root + its state transitions, with timings and the
# rollback path marked), the same view the Jaeger UI shows.
#
# Usage:
#   scripts/show-trace.sh [--jaeger URL] [--service NAME] [--limit N]
# Defaults: --jaeger http://localhost:16686  --service fraisier  --limit 50
#
set -euo pipefail

JAEGER="${FRAISIER_JAEGER_URL:-http://localhost:16686}"
SERVICE="fraisier"
LIMIT="50"
while [ $# -gt 0 ]; do
  case "$1" in
    --jaeger)  JAEGER="$2"; shift 2;;
    --service) SERVICE="$2"; shift 2;;
    --limit)   LIMIT="$2"; shift 2;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

command -v curl    >/dev/null || { echo "error: curl not found" >&2; exit 1; }
command -v python3 >/dev/null || { echo "error: python3 not found" >&2; exit 1; }

# Fetch to a temp file so python can read the program (heredoc on stdin) and the
# trace JSON (a path arg) without the two contending for stdin.
TRACE_JSON="$(mktemp)"
trap 'rm -f "$TRACE_JSON"' EXIT
curl -fsS "$JAEGER/api/traces?service=$SERVICE&limit=$LIMIT" > "$TRACE_JSON" 2>/dev/null \
  || { echo "error: could not reach Jaeger at $JAEGER (is it running?)" >&2; exit 1; }

python3 - "$SERVICE" "$TRACE_JSON" <<'PY'
import json, sys
service, path = sys.argv[1], sys.argv[2]
with open(path) as fh:
    data = json.load(fh).get("data", [])
if not data:
    print(f"no traces for service '{service}'")
    sys.exit(0)

def tag(span, key):
    for t in span.get("tags", []):
        if t["key"] == key:
            return t["value"]
    return None

for t in sorted(data, key=lambda t: min(s["startTime"] for s in t["spans"])):
    spans = t["spans"]
    roots = [s for s in spans if not s.get("references")]
    root = roots[0] if roots else min(spans, key=lambda s: s["startTime"])
    t0 = root["startTime"]
    fr, env = tag(root, "deploy.fraise"), tag(root, "deploy.environment")
    trans = sorted((s for s in spans if s["operationName"] == "saga.state_transition"),
                   key=lambda s: s["startTime"])
    final = tag(trans[-1], "deploy.to_state") if trans else None
    verdict = {"committed": "COMMITTED", "rolled_back": "ROLLED BACK"}.get(final, final or "?")
    print(f"\n■ trace {t['traceID'][:16]}  {fr}/{env}  ({root['duration']/1000:.0f}ms, {len(spans)} spans)")
    print(f"  saga.deploy  →  {verdict}")
    for s in trans:
        off = (s["startTime"] - t0) / 1000.0
        frm, to = tag(s, "deploy.from_state"), tag(s, "deploy.to_state")
        comp = "  ⮌" if to and to.startswith("compensating") else ""
        print(f"    +{off:6.0f}ms  {str(frm):>24}  →  {to}{comp}")
PY

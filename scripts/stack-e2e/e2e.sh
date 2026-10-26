#!/usr/bin/env bash
# Stack acceptance on the throwaway host: one `fraisier stack deploy` brings
# two FraiseQL 2.18 subgraphs and Hive Router up on PG 18 + pg_tviews, a
# federated query answers from both, and `stack status` reports every member.
set -uo pipefail
pass(){ printf 'PASS %s\n' "$*"; }
fail(){ printf 'FAIL %s\n' "$*"; FAILED=1; }
say(){ printf '\n=== %s\n' "$*"; }
FAILED=0
E=/srv/erp
F="$(cd "$(dirname "$0")" && pwd)"
W="${WORK:-/var/tmp/fraisier-stack-e2e}"
O="$W"/o
export ERP_DSN='postgresql://erp:erp-pw@127.0.0.1/erp'
SD(){ (cd $E/conf && fraisier stack deploy --stack stack.staging.toml --state-dir $E/state "$@"); }
ST(){ (cd $E/conf && fraisier stack status --stack stack.staging.toml --state-dir $E/state); }
# An empty reply (router down) prints a sentinel, so no jq check passes on it.
Q(){ curl -s -X POST -H 'content-type: application/json' --data "{\"query\":\"$1\"}" http://127.0.0.1:4000/graphql || true; }
J(){ [ -n "$1" ] && echo "$1" | jq -e "$2" >/dev/null; }
release(){ # <dir> <variant>
  rm -rf "$1"; mkdir -p "$1"
  cp "$W/out/$2.compiled.json" "$1/schema.compiled.json"
  cp "$F"/server.config.toml "$1/"
  cp "$F/$2.up.sql" "$1/up.sql"
}

for v in 1 2; do [ -s "$W/supergraph-v$v.graphql" ] || { echo "compose $W/supergraph-v$v.graphql first (README.md)"; exit 2; }; done
cp "$W/supergraph-v1.graphql" $E/src/gateway/supergraph.graphql

say "1. one stack deploy: orders, users, then the gateway"
SD --app-version users=v1 --app-version orders=v1 --app-version gateway=v1 > "$O" 2>&1; rc=$?; cat "$O"
if [ $rc = 0 ]; then pass "stack deploy exits 0"; else fail "stack deploy rc=$rc"; fi
if [ "$(awk '$1 == "deployed" {print $2}' "$O" | tr '\n' ' ')" = "orders users gateway " ]; then pass "deploy order: orders, users, gateway"; else fail "order"; fi
if grep -q 'sdl users: matches' "$O" && grep -q 'sdl orders: matches' "$O"; then pass "both SDLs match the router's candidate release"; else fail "sdl after deploy"; fi
if systemctl is-active -q erp-users erp-orders erp-gateway; then pass "all three units running"; else fail "units"; fi

say "2. a federated query through Hive Router answers from both subgraphs"
r=$(Q '{ users { name email } orders { total customerName } }'); echo "$r"
if J "$r" '.data.users | map(.name) | sort == ["Ada","Grace"]'; then pass "users from the users subgraph"; else fail "users"; fi
if J "$r" '.data.orders | map(.total) | sort == [7.25,42.5]'; then pass "orders from the orders subgraph"; else fail "orders"; fi
if J "$r" '.errors == null'; then pass "no errors"; else fail "errors"; fi

say "3. stack status lists every member with its health"
ST > "$O" 2>&1; rc=$?; cat "$O"
if [ $rc = 0 ]; then pass "status exits 0"; else fail "status rc=$rc"; fi
for m in users orders gateway; do if grep -Eq "^  $m +ok +v1 +committed" "$O"; then pass "$m ok at v1"; else fail "$m row"; fi; done
if grep -q 'database postgresql://\*\*\*@127.0.0.1/erp (orders, users): ready' "$O"; then pass "one database row, ready, for both subgraphs"; else fail "db row"; fi
(cd $E/conf && fraisier --json stack status --stack stack.staging.toml --state-dir $E/state) > "$O.json" 2>&1
if jq -e '.healthy == true and (.members | length == 3) and (.sdl | all(.matches))' "$O.json" >/dev/null 2>&1; then
  pass "json healthy, three members, every SDL matches"
else head -c 800 "$O.json"; fail "json"; fi

say "4. a subgraph redeployed with a new schema under the old supergraph: status catches it"
release $E/src/users users-v2
(cd $E/conf && fraisier deploy --config users.staging.toml --state-dir $E/state --app-version v2) > "$O" 2>&1; rc=$?; tail -2 "$O"
if [ $rc = 0 ]; then pass "users v2 deployed alone"; else fail "users v2 rc=$rc"; fi
ST > "$O" 2>&1; rc=$?; cat "$O"
if [ $rc = 1 ] && grep -q 'sdl users: DIFFERS' "$O" && grep -q nickname "$O"; then pass "status fails naming users' SDL and the new field"; else fail "drift rc=$rc"; fi
if grep -q 'sdl orders: matches' "$O"; then pass "orders still matches"; else fail "orders sdl"; fi

say "4b. not recomposed: the stack deploy refuses users v2 in its health step; the router never deploys"
SD --app-version users=v2a --app-version orders=v2a --app-version gateway=v2a > "$O" 2>&1; rc=$?; cat "$O"
if [ $rc = 1 ] && grep -Eq 'failed +users: .*rolled_back' "$O" && grep -Eq 'skipped +gateway' "$O"; then pass "users rolled itself back; gateway skipped"; else fail "prevent rc=$rc"; fi
if grep -q 'sdl users: DIFFERS' "$O" && grep -q nickname "$O"; then pass "the report names users' SDL and the new field"; else fail "prevent sdl"; fi
if [ "$(readlink $E/gateway/current)" = "$E/gateway/releases/v1" ]; then pass "the gateway still serves v1"; else fail "gateway moved"; fi

say "5. recomposed: the stack deploys the new router release and matches again"
cp "$W"/out/users-v2.graphql $E/src/gateway/sdl/users.graphql
cp "$W"/supergraph-v2.graphql $E/src/gateway/supergraph.graphql
SD --app-version users=v2b --app-version orders=v2 --app-version gateway=v2 > "$O" 2>&1; rc=$?; cat "$O"
if [ $rc = 0 ] && grep -q 'sdl users: matches' "$O"; then pass "stack deploy matches after recomposing"; else fail "recompose rc=$rc"; fi
r=$(Q '{ users { name nickname } }'); echo "$r"
if J "$r" '.data.users | length == 2 and all(.nickname == "n/a")'; then pass "the new field answers through the router"; else fail "nickname"; fi

say "6. an unhealthy member fails the status"
systemctl stop erp-orders
ST > "$O" 2>&1; rc=$?; cat "$O"
if [ $rc = 1 ] && grep -Eq '^  orders +UNHEALTHY' "$O"; then pass "orders reported unhealthy, exit 1"; else fail "unhealthy rc=$rc"; fi
systemctl start erp-orders

say "7. a failing member stops the stack; the members after it never start"
cp $E/src/orders/up.sql "$W"/orders-up.good
echo 'SELECT no_such_function();' >> $E/src/orders/up.sql
SD --app-version users=v3 --app-version orders=v3 --app-version gateway=v3 > "$O" 2>&1; rc=$?; cat "$O"
if [ $rc = 1 ] && grep -Eq 'failed +orders' "$O" && grep -Eq 'skipped +users' "$O" && grep -Eq 'skipped +gateway' "$O"; then pass "orders failed; users and gateway skipped"; else fail "stop rc=$rc"; fi
if grep -q 'sdl' "$O"; then fail "an SDL check ran after a failure"; else pass "no SDL check after a failure"; fi
if r=$(Q '{ orders { total } }'); J "$r" '.data.orders | length == 2'; then pass "the stack still answers (orders rolled itself back)"; else fail "after failure: $r"; fi
cp "$W"/orders-up.good $E/src/orders/up.sql

say "7b. a router release whose supergraph cannot load fails its readiness and rolls back"
cp $E/src/gateway/supergraph.graphql "$W"/supergraph.good
echo 'this is not a supergraph' > $E/src/gateway/supergraph.graphql
(cd $E/conf && fraisier deploy --config gateway.staging.toml --state-dir $E/state --app-version broken) > "$O" 2>&1; rc=$?; tail -2 "$O"
if [ $rc != 0 ] && grep -q ' rolled_back' "$O"; then pass "the broken router release rolled back"; else fail "broken router rc=$rc"; fi
if r=$(Q '{ users { name } }'); J "$r" '.data.users | length == 2'; then pass "the previous router release still answers"; else fail "after rollback: $r"; fi
cp "$W"/supergraph.good $E/src/gateway/supergraph.graphql

say "8. a database without pg_tviews is not ready"
runuser -u postgres -- psql -X -q -c 'DROP DATABASE IF EXISTS erp_plain' -c 'CREATE DATABASE erp_plain OWNER erp'
ERP_DSN='postgresql://erp:erp-pw@127.0.0.1/erp_plain' ST > "$O" 2>&1; rc=$?; cat "$O"
if [ $rc = 1 ] && grep -q 'NOT READY' "$O" && grep -q pg_tviews "$O"; then pass "the DB row names pg_tviews"; else fail "readiness rc=$rc"; fi

say "9. no password in any output"
if grep -rl 'erp-pw' "$W"/o*; then fail "a password leaked"; else pass "no password"; fi

say "RESULT: $([ $FAILED = 0 ] && echo ALL PASSED || echo FAILURES)"

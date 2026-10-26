# Stack acceptance: two FraiseQL subgraphs and Hive Router

The end-to-end proof for `fraisier stack deploy` and `fraisier stack status`
([docs/stacks.md](../../docs/stacks.md)). On a host prepared by
[`provision-pg18-tviews.sh`](../provision-pg18-tviews.sh), one stack deploy brings
up two FraiseQL 2.18 subgraphs (`users`, `orders`) and Hive Router in front of
them. A federated query then answers from both. Then the checks below run.

| Step | What it proves |
|---|---|
| 1 | One `stack deploy` deploys `orders`, `users`, then `gateway`, and both SDLs match the router's release. |
| 2 | `{ users { … } orders { … } }` through the router answers from both subgraphs. |
| 3 | `stack status` (text and `--json`) shows every member healthy at its version, and one ready row for the shared database. |
| 4 | A subgraph redeployed alone with a new schema, under the old supergraph, makes `stack status` fail with `sdl users: DIFFERS`, naming the new field. |
| 5 | Recomposed, the stack deploys again, the SDLs match, and the new field answers through the router. |
| 6 | A stopped subgraph makes `stack status` exit 1, naming it. |
| 7 | A failing migration stops the stack at that member. The members after it are skipped, and the stack still answers. |
| 7b | A router release whose supergraph cannot load fails `/readiness` and rolls back, and the previous release still answers. |
| 8 | A database without pg_tviews is reported `NOT READY`, naming it. |
| 9 | No password appears in any output. |

## Running it

Everything runs as root on a throwaway Debian 12 host: the scripts drop and
create objects in the `erp` database and replace `/srv/erp`. A `ccx33` builds
everything in about 15 minutes.

1. **Prepare the host.** Run `scripts/provision-pg18-tviews.sh --database erp`.
2. **Build the binaries on the host.** Binaries built elsewhere may need a newer
   glibc than Debian 12 has.
   - fraisier: `cargo build --release -p fraisier`.
   - FraiseQL at the tag in [`tools/fraiseql-version`](../../tools/fraiseql-version):
     `cargo build --release -p fraiseql-server -p fraiseql-cli --features fraiseql-server/federation,fraiseql-cli/federation`.
   - Hive Router from <https://github.com/graphql-hive/router>, last verified at
     `hive-router/v0.3.6`: `cargo build --release --bin hive_router`. Its build
     script runs `npm install`, so the host needs `npm`.

   Install `fraisier`, `fraiseql-server`, `fraiseql-cli` and `hive_router` on
   `PATH`. The host also needs `jq` and `curl`.
3. **Set up.** Run `bash setup.sh`. It compiles the three subgraph schemas
   (`users` v1 and v2, `orders` v1) into `$WORK/out` and prints each one's
   `_service` SDL there. `$WORK` defaults to `/var/tmp/fraisier-stack-e2e`. The
   script also lays out the releases, systemd units and fraisier configs under
   `/srv/erp`.
4. **Compose the two supergraphs.** fraisier never composes, so this is the
   application build's step. Use any Apollo Federation v2 composer, with each
   subgraph's real routing URL. For example, with `@apollo/composition`'s
   `composeServices`, write the result to `$WORK`:

   | Supergraph | Subgraphs |
   |---|---|
   | `supergraph-v1.graphql` | `users` from `out/users-v1.graphql` at `http://127.0.0.1:8101/graphql`, and `orders` from `out/orders-v1.graphql` at `http://127.0.0.1:8102/graphql` |
   | `supergraph-v2.graphql` | the same, with `out/users-v2.graphql` |
5. **Run** `bash e2e.sh`. Its last line is `RESULT: ALL PASSED` or
   `RESULT: FAILURES`.

## Findings it encodes

- **The router's health is `/readiness`.** Hive Router answers `/health` with
  200 while it has no supergraph it can serve, so a deploy probing `/health`
  would commit a router that serves nothing.
- **The router's no-op migration prints a constant revision.** One that prints
  none makes every failed router deploy a `partial_rollback`, because migrate
  cannot undo an "initial" migration.
- **The subgraphs' database role needs `pg_read_all_settings`.** Without it,
  readiness cannot see that pg_tviews is preloaded, so it refuses the deploy and
  names the grant.

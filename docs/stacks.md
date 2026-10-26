# Stacks

A stack is one environment's federated application: the subgraphs, the router
in front of them, and the databases they use. `fraisier stack deploy` deploys
its members in dependency order. `fraisier stack status` reports them as one.

A stack adds nothing to a deployable. Each member stays an ordinary
`fraisier.toml`, and `fraisier deploy --config <member>` keeps working on its
own.

## The stack file

One file per environment, `stack.<env>.toml`, references the members' configs
by path:

```toml
[stack]
name = "erp"
environment = "staging"
router = "gateway"            # optional: enables the SDL check

[[stack.member]]
config = "management.staging.toml"
sdl = "federation/sdl/management.graphql"
graphql_url = "http://127.0.0.1:8042/graphql"
sdl_headers = { "x-api-key" = "MANAGEMENT_SERVICE_TOKEN" }

[[stack.member]]
config = "billing.staging.toml"
sdl = "federation/sdl/billing.graphql"
graphql_url = "http://127.0.0.1:8043/graphql"

[[stack.member]]
config = "gateway.staging.toml"
depends_on = ["management", "billing"]
```

| Key | Meaning |
|---|---|
| `[stack].name`, `environment` | Required. Every member config's `[deploy].environment` must equal `environment`, so a production config cannot sit in a staging stack. |
| `[stack].router` | The member that is the router. Its `[artifact].active_path` is the release every `sdl` path is read from. |
| `config` | The member's config, relative to the stack file. The member's name is that config's `[deploy].name`; the stack file never restates it. |
| `depends_on` | Members (by name) that deploy first. |
| `sdl` | The member's SDL as the router's release holds it: a relative path inside that release. Needs `router` and `graphql_url`. |
| `graphql_url` | The member's GraphQL endpoint, asked for `{ _service { sdl } }`. |
| `sdl_headers` | Headers for that query. Each value names the environment variable that holds it, so a credential never sits in the file. |

These are errors, all reported together: a cycle, a `depends_on` naming no
member, two members with the same name, a member config that cannot be read,
and any `sdl` setting that is missing what it needs.

## The router is a member

The router is an ordinary deployable. fraisier does not compose the supergraph:
the application's build composes it from the committed SDL files and ships it in
the router's release. For Hive Router serving a committed supergraph:

```toml
[deploy]
name = "gateway"
environment = "staging"

[artifact]
source = "local"
path = "/srv/erp/_src/erp"                # holds federation/supergraph.graphql
active_path = "/srv/erp/gateway/current"
staging_dir = "/srv/erp/gateway/releases"

# The router owns no database; the migration axis is a no-op. It still prints
# a constant revision: one that prints none makes every failed router deploy a
# partial_rollback, because migrate cannot undo an "initial" migration.
[migration]
adapter = "command"
preflight_mode = "off"

[migration.settings.commands]
current_revision = "echo none"
up = "true"
down_to = "true"

[service]
adapter = "systemd"
unit = "erp-gateway.service"

# /readiness, not /health: Hive Router answers /health with 200 while it has no
# supergraph it can serve, and /readiness with 503.
[health]
adapter = "http"
url = "http://127.0.0.1:4000/readiness"
```

Probe `/readiness`: a router release whose supergraph does not load then fails
its health step, and the deploy rolls back to the previous release.

## `fraisier stack deploy`

```sh
fraisier stack deploy --stack stack.staging.toml --app-version gateway=v12
```

Members deploy one at a time. Each member deploys after everything it depends
on, and ties go to the lexicographically first name, so the order never depends
on the file's order. Each member deploys exactly as `fraisier deploy` would,
with its own preflight, saga and rollback. `--app-version <member>=<version>`
can repeat; `--dry-run` and `--skip-preflight` apply to every member.

The first member that fails stops the stack:

- the failed member has rolled itself back;
- the members after it never start;
- **the members before it stay deployed.** There is no rollback across members.

The report names each member as `deployed`, `failed` or `skipped`, and the
command exits 1.

Once every member has deployed, the SDL check runs (below). A difference exits
1, but nothing is rolled back: each deploy succeeded on its own terms.

## `fraisier stack status`

```sh
fraisier stack status --stack stack.staging.toml
```

```text
stack erp (staging): UNHEALTHY
  billing     ok         v1.4.0  committed 2026-10-26T10:00:00Z
  gateway     ok         v12     committed 2026-10-26T10:02:11Z
  management  UNHEALTHY  v2.0.1  committed 2026-10-26T10:01:40Z
      localhost: HTTP 503 from /health
  database postgres://***@127.0.0.1/erp (billing, management): ready
  sdl billing: matches
  sdl management: matches
```

For each member, status shows:

- its health, from its own `[health]` check, the one `fraisier health` runs. A
  member whose health cannot be checked is unhealthy;
- its live version, from the committed release ledger;
- its last recorded deploy.

Below the members:

- **Databases.** Each database that members declare `[database.requires]` for
  gets one row per distinct DSN. The row is probed against every declaring
  member's requirements, the same probe a deploy's preflight runs (see
  [database-requirements.md](database-requirements.md)).
- **SDL.** With a router, each subgraph's SDL check is listed.

The command exits 1 when a member is unhealthy, a database is not ready or an
SDL differs. The last deploy is reported and never judged. `--json` gives the
same as an object.

## The SDL check

A subgraph deployed with a new schema, under a router that still serves the old
supergraph, is a silent break. For every member with `sdl`, fraisier:

1. reads the file at `<router active_path>/<sdl>`;
2. asks the member's `graphql_url` for `{ _service { sdl } }`, with
   `sdl_headers`;
3. compares the two exactly, except that trailing newlines are not a
   difference.

A difference (`DIFFERS`) names the first line where the two differ. To fix it,
recapture the subgraph's SDL, recompose, and deploy the router. A member that
cannot be checked (`UNCHECKED`) is a failure too: the file is missing, a
header's variable is unset, or the query fails. Header values never appear in
output.

The check reads the router's release on the host fraisier runs on.

## The target

A stack deploys onto a host that is already prepared.
`scripts/provision-pg18-tviews.sh` installs PostgreSQL 18 and pg_tviews, once,
as root. `fraisier stack deploy` then does the rest as the deploy user.

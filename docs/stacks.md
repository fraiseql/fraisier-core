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

[[stack.member]]
config = "billing.staging.toml"
sdl = "federation/sdl/billing.graphql"

[[stack.member]]
config = "gateway.staging.toml"
depends_on = ["management", "billing"]
```

| Key | Meaning |
|---|---|
| `[stack].name`, `environment` | Required. Every member config's `[deploy].environment` must equal `environment`, so a production config cannot sit in a staging stack. |
| `[stack].router` | The member that is the router. Its release holds every `sdl` path. It needs `[artifact].active_path`, and it must deploy after every member with `sdl` (list them in its `depends_on`). |
| `config` | The member's config, relative to the stack file. The member's name is that config's `[deploy].name`; the stack file never restates it. |
| `depends_on` | Members (by name) that deploy first. |
| `sdl` | The member's SDL as the router's release holds it: a relative path inside that release. Needs `router`, and `[federation].graphql_url` in the member's config. |

The stack file never restates a member's properties. A member's GraphQL
endpoint is in its own config:

```toml
# management.staging.toml
[federation]
graphql_url = "http://127.0.0.1:8042/graphql"   # asked for { _service { sdl } }
```

The query sends that member's `[health]` headers and the header its
`[health].token_provider` resolves, so a subgraph's credentials are configured
once. A stack file that still sets `graphql_url` or `sdl_headers` is refused,
with a pointer to where each went.

These are errors, all reported together: a cycle, a `depends_on` naming no
member, two members with the same name, a member config that cannot be read,
any `sdl` setting that is missing what it needs, a router that deploys before
a member with `sdl`, and a member with `sdl` that deploys blue-green (its
health step probes the green fleet, which the SDL check does not gate).

## The router is a member

The router is an ordinary deployable. fraisier does not compose the supergraph:
the application's build composes it from the committed SDL files and ships it in
the router's release. For Hive Router serving a committed supergraph:

<!-- router-example -->
```toml
[deploy]
name = "gateway"
environment = "staging"

[artifact]
source = "local"
path = "/srv/erp/_src/erp"                # holds federation/supergraph.graphql
active_path = "/srv/erp/gateway/current"
staging_dir = "/srv/erp/gateway/releases"

# The router owns no database.
[migration]
adapter = "none"

[service]
adapter = "systemd"
unit = "erp-gateway.service"

# /readiness, not /health: Hive Router answers /health with 200 while it has no
# supergraph it can serve, and /readiness with 503.
[health]
adapter = "http"
url = "http://127.0.0.1:4000/readiness"
```

Two rules for the router member:

- **Probe `/readiness`, not `/health`.** A router release whose supergraph does
  not load then fails its health step, and the deploy rolls back to the
  previous release.
- **Declare that it owns no database: `[migration] adapter = "none"`.** The
  saga then knows there is nothing to compensate on that axis, so a failed
  router deploy ends `rolled_back`, not `partial_rollback` for want of a
  revision to return to.

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

With a router, the router's candidate release is staged first, and each
subgraph's health step checks its SDL against it (below). A subgraph that
differs fails like any unhealthy release, so the router is never deployed
over it.

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
- **SDL.** With a router, each subgraph's SDL is checked against the router's
  active release and listed.

The command exits 1 when a member is unhealthy, a database is not ready or an
SDL differs. The last deploy is reported and never judged. `--json` gives the
same as an object.

## The SDL check

A subgraph deployed with a new schema, under a router that still serves the old
supergraph, is a silent break. `stack deploy` prevents it:

1. Before any member deploys, it stages the router's release at its
   `--app-version`, through the router's own artifact adapter, without
   activating it. This is the *candidate* release. If it cannot be staged,
   nothing deploys.
2. Each member with `sdl` deploys as usual. Once its own health probe passes,
   its health step:
   - reads `<candidate>/<sdl>`;
   - asks the member's `[federation].graphql_url` for `{ _service { sdl } }`,
     with its `[health]` headers and token;
   - compares the two exactly, except that trailing newlines are not a
     difference.
3. A difference (`DIFFERS`), or a check that cannot run (`UNCHECKED`: the file
   is missing, the token provider fails, the query fails), fails that health
   step. The member's saga rolls it back, the stack stops, and the router is
   never deployed.
4. The router deploys last and stages the same release again.

A difference names the first line where the two differ. To fix it, recapture
the subgraph's SDL, recompose, and deploy the stack. Header values never appear
in output.

`stack deploy --dry-run` stages nothing and asks no SDL; it reports
`sdl <member>: not checked (dry run)`. A member whose own dry run passed is
listed as `planned`, never `deployed`.

The candidate is read on the host fraisier runs on. When the router deploys
through `[hosts]`, its releases are on its hosts, so nothing is staged locally:
each checked member deploys unchecked, is reported `UNCHECKED` with that
reason, and the command exits 1. A router whose artifact stages as a file, not
a directory, is reported the same way.

`stack status` runs the same comparison after the fact, against the router's
*active* release: it detects a subgraph deployed on its own, outside the stack.

## The target

A stack deploys onto a host that is already prepared.
`scripts/provision-pg18-tviews.sh` installs PostgreSQL 18 and pg_tviews, once,
as root. `fraisier stack deploy` then does the rest as the deploy user.

# FraiseQL 2.18.0 `/health` captures

These are the exact response bodies a `fraiseql-server` 2.18.0 sent. They were not
written by hand. The server was built from the `v2.18.0` tag with
`--features observers`, started with `FRAISEQL_ENV=production`, and run against a
scratch PostgreSQL database. Every capture answered **HTTP 200**.

| File | State | Server config | Database |
|---|---|---|---|
| `health-not-configured.json` | Capability not configured | No `[observers]` | — |
| `health-configured-not-started.json` | Configured, but failed to start | `[observers] enabled = true` | Observer tables absent; the log says `relation "tb_observer" does not exist` |
| `health-running.json` | Configured and running | `[observers] enabled = true` | fraiseql-observers' migrations applied |

All three use a schema compiled by `fraiseql-cli` 2.18.0 from
`examples/basic/schema.json`. The two observer captures have `[observers]
enabled = true` in the compile-time `fraiseql.toml` as well.

## What the captures show

- A capability that is not configured has no `started` key at all.
- A configured capability that failed to start reports `started: false` and
  `healthy: false`. The top-level `status` reads `degraded`, but **the status
  code is still 200**. A check that looks only at the status code passes it.
- `observers` never reports `serving`, not even while it is running: it mounts
  no routes when there is no `[auth]` configuration. A rule that required
  `serving` would therefore fail a healthy observer runtime.

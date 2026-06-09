# fraisier-adapter-docker-compose

The Docker Compose `ServiceAdapter` for fraisier. It restarts and reports the
status of a Compose service by shelling out to the `docker compose` CLI
(PRD §6.3).

## Configuration

Read from the `[service]` settings table:

```toml
[service]
adapter = "docker-compose"
compose_service = "web"                 # the service within the project (required)
compose_file = "/srv/app/compose.yaml"  # optional; the Compose default is used if omitted
```

- `restart` → `docker compose [-f <file>] restart <service>` (non-zero exit =
  failure).
- `status` → `docker compose [-f <file>] ps --format json <service>`; `running`
  is read from each container's `State` (`"running"`) or `Status` (`Up …`),
  tolerating an NDJSON stream, a JSON array, or the legacy plain-text table.

## v2 vs v1

By default the adapter spawns `docker` and prepends the `compose` subcommand
(v2). Point it at a legacy `docker-compose` standalone binary — via
`FRAISIER_DOCKER_BIN` or `with_program` — and it drops the subcommand
automatically, inferred from the program's basename.

## Integration testing

Real Compose exercises need a Docker daemon; the argv construction and `ps`
parsing are unit-tested here on any platform.

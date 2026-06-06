# fraisier-config

Parser, **SpecQL-preset** expander, and validator for the native `fraisier.toml`
deploy configuration (PRD §7.1 / §7.1a).

This crate is the bridge between the on-disk config file and the **frozen**
vocabulary types in [`fraisier-core`](../fraisier-core). It depends on
`fraisier-core` for those types only and never on `fraisier-ipc` — concrete
adapter selection is wired at the CLI layer (the crate-graph rule).

## What it does

- **Parse** `[deploy]`, `[hosts]`, `[artifact]`, `[migration]`, `[service]`,
  `[health]`, and `[lb]` into a [`DeployConfig`].
- **Expand** the one-line `[specql]` preset (PRD §7.1a) into a full config at
  load time. Explicit fields always win over preset defaults.
- **Validate** in a separate pass: cross-field, value-domain, and
  per-adapter-requirement checks, each with a located `section.field` path and a
  helpful message. Validation is pure — it performs no filesystem or network I/O.

## Secret handling (Decision 5)

`[migration].database_url_env` names the *source* environment variable that holds
the database DSN. The config maps it onto `AdapterCtx.env_secrets["DATABASE_URL"]`
(the logical name); the adapter then resolves the value via
`AdapterCtx::secret("DATABASE_URL")`. The DSN value itself never enters the config
file, argv, or JSON params.

## The SpecQL preset

`[specql]` fills Fraise-stack-conventional defaults for an app deployed via
SpecQL:

| Axis      | Preset default                                              |
|-----------|------------------------------------------------------------|
| migration | `confiture`, `forward_compatible_lint = true`, `migrations_path` = `<schema-dir>/migrations`, `database_url_env = "DATABASE_URL"` |
| service   | `systemd`, `unit = "<name>.service"`                        |
| health    | `http`, `url = "http://{host.address}:8080/health"`, `200` |
| artifact  | `local`, `path = "./target/release"` (override with an explicit `[artifact]` for release-based deploys) |
| hosts/lb  | populated only when `hosts` lists more than one address     |

The preset takes a `name` field (the deployable's name) because `[deploy].name`
is required and is not read from the SpecQL `schema.toml`. The `schema` path is
recorded but not parsed.

## Example

```rust
use fraisier_config::DeployConfig;

let cfg = DeployConfig::load(
    r#"
    [deploy]
    name = "fraiseql"
    environment = "production"

    [artifact]
    source = "release"
    release_url = "https://example.com/app-{version}.tar.gz"
    checksum_url = "https://example.com/app-{version}.tar.gz.sha256"

    [migration]
    adapter = "confiture"
    database_url_env = "FRAISEQL_DATABASE_URL"

    [service]
    adapter = "systemd"
    unit = "fraiseql.service"

    [health]
    adapter = "http"
    url = "http://127.0.0.1:8080/health"
    "#,
)
.expect("valid config");

assert_eq!(
    cfg.migration_env_secrets()["DATABASE_URL"],
    "FRAISEQL_DATABASE_URL",
);
```

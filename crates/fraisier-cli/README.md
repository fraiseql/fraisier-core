# fraisier-cli

The `fraisier` command-line binary. Per the crate-graph rule this is the layer
that depends on **both** `fraisier-core` (the deploy composition + adapter axes)
and `fraisier-ipc` (external adapters), and it is where in-process vs IPC adapter
selection is wired (see `src/factory.rs`).

## Commands

```
fraisier validate-config [--config <path>]
fraisier deploy          [--config <path>] [--state-dir <dir>] [--host <addr>] [--app-version <v>] [--dry-run]
fraisier status          [--config <path>] [--state-dir <dir>]
fraisier adapter list
fraisier adapter describe <name>

# global: --json emits machine-readable output for any command
```

- **validate-config** — parse, expand the `[specql]` preset, and run the
  validation pass; prints every located issue. Exit 0 if valid (warnings
  allowed), 1 otherwise.
- **deploy** — validate, then resolve the adapter plan. With `--dry-run` it prints
  the plan and exits without touching anything; otherwise it runs the single-host
  deploy (`fraisier-core::single_host`) against the filesystem state store.
- **status** — the recorded saga state and release ledger for the config's deploy.
- **adapter list / describe** — discover `fraisier-adapter-*` binaries on `PATH`
  and run the JSON-RPC `describe` handshake against one.

## Adapter selection (the crate-graph wiring point)

The migration adapter name in `[migration].adapter` decides the path:

| Name                | Resolution                                              |
|---------------------|---------------------------------------------------------|
| `confiture`         | in-process `ConfitureMigration`                         |
| `command`           | in-process `CommandMigration` (from `[migration.settings]`) |
| anything else       | IPC: spawn `fraisier-adapter-<name>`                    |

The two paths handle the `DATABASE_URL` secret differently, both honouring
Decision 5: in-process adapters resolve the *source* env var named by
`database_url_env` themselves; for IPC the CLI resolves the value and injects it on
the child under the logical name, so the source var name never crosses the process
boundary.

## Exit codes

`0` success · `1` invalid config / clean rollback / not-found · `2` partial
rollback (operator intervention required).

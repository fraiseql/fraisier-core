# fraisier-adapter-command

The universal **escape-hatch** migration adapter. It implements the frozen
`MigrationAdapter` trait by running user-configured shell commands, so any
migration tool fraisier doesn't natively wrap can still be deployed through the
same contract.

## Configuration

Built from the `[migration.command]` settings table via
`CommandMigration::from_settings`. Commands live under `commands`; each is a
shell string (`sh -c`) or an argv array (no shell):

```toml
[migration.command.commands]
current_revision = "mytool current --quiet"   # prints the revision (empty = none)
up               = "mytool migrate up"          # non-zero exit = failure
down_to          = ["mytool", "migrate", "down"]
verify           = "mytool check"               # non-zero exit = failed check (a result, not an error)
```

`describe` advertises only the configured commands, so the command set is fixed
at construction.

## Secrets and target (never in argv)

Declared secrets (`AdapterCtx::env_secrets`) are resolved and exported to the
command's environment under their logical names (read `$DATABASE_URL`, etc.). The
`up`/`down_to` target revision is exported as `$FRAISIER_TARGET`. Nothing secret
is ever placed in argv — consistent with Phase 1 review Decision 5.

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

## Working directory and the release context

On a single-host deploy the commands run **from the staged release directory**
(the release `fetch` just cut, `StagedArtifact.path`), not the directory
`fraisier deploy` was invoked from. So a relative command resolves against the
release — this is the idiomatic **source-run / build-on-deploy** shape:

```toml
[migration.command.commands]
# resolves against the staged release, whatever --app-version is
up = "bash scripts/deploy/prepare.sh"
```

Each command also receives the release context in its environment, so a script
can reference the deploy's paths without hard-coding them:

| Variable               | Value                                                        |
| ---------------------- | ----------------------------------------------------------- |
| `FRAISIER_RELEASE_DIR` | the command's working directory (the staged release)        |
| `FRAISIER_ACTIVE_PATH` | the `active_path` symlink target, when `[artifact]` sets it |
| `FRAISIER_APP_VERSION` | the version being deployed, when known                      |

`FRAISIER_ACTIVE_PATH` / `FRAISIER_APP_VERSION` are unset (not empty) when their
settings are absent. Multi-host deploys migrate once on the orchestrator, where
the per-host release is not present, so there the working directory is the base
directory rather than a release.

> **PATH under `sudo`.** Commands inherit fraisier's environment. When the deploy
> is launched with `sudo`, sudo's `secure_path` can drop `/usr/local/bin`, so
> operator-installed tools (e.g. `uv`, language CLIs) may not be found. Set an
> explicit `PATH` at the top of your prepare script, or run fraisier so that its
> `PATH` already contains the tools.

## Secrets and target (never in argv)

Declared secrets (`AdapterCtx::env_secrets`) are resolved and exported to the
command's environment under their logical names (read `$DATABASE_URL`, etc.). The
`up`/`down_to` target revision is exported as `$FRAISIER_TARGET`. Nothing secret
is ever placed in argv.

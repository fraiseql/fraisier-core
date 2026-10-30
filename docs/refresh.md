# Refreshing an environment from another one's backups

`fraisier db refresh` replaces a non-production environment's database with
the newest archive of another environment. It drops a database, so it only
ever drops one that was marked for it, by a config that declares itself
non-production.

## Marking a refresh target

The refresh target's config declares `[deploy] production = false` and has a
`[refresh]` section. Mark its database once:

```sh
fraisier db mark-refresh-target --config /etc/fraisier/checkout-staging.toml
```

This sets the database-wide setting `fraisier.refresh_target` to
`<fraise>/<environment>` (here `checkout/staging`). Marking again for the same
config changes nothing. It refuses, and changes nothing, when:

- the config does not declare `[deploy] production`: an undeclared key is never
  taken to mean "not production";
- the config says `production = true`;
- the config has no `[refresh]` section;
- the database is already marked for another config. Clearing that marker is a
  deliberate act: `ALTER DATABASE "<db>" RESET fraisier.refresh_target`;
- the role cannot set the marker (below).

`fraisier doctor` reports the same facts as its `refresh_target` check.

### PostgreSQL 15 and later: one GRANT

From PostgreSQL 15, a role that is not a superuser may not set a custom
parameter such as `fraisier.refresh_target` on a database, even one it owns.
A superuser runs, once per role:

```sql
GRANT SET ON PARAMETER fraisier.refresh_target TO app;
```

The refresh re-creates the database and marks it again, so the role needs this
privilege for every refresh, not only the first marking. Both
`mark-refresh-target` and `doctor` check it and print the exact statement.

## Running a refresh

```sh
fraisier db refresh --config /etc/fraisier/checkout-staging.toml        # the plan
fraisier db refresh --config /etc/fraisier/checkout-staging.toml --yes  # do it
```

The refresh runs on the one host it is invoked on, for a config whose
`[service]` uses the `systemd` adapter. Before anything stops, it refuses, with
nothing changed, when:

- the config is not a refresh target (above), deploys to `[hosts]`, or its
  service is not a systemd unit;
- there is no archive of `<fraise>-<source_environment>` in
  `[refresh].source_dir`, the newest does not read back, it is older than
  `max_age_hours` (48 by default), or it is stamped in the future;
- `[refresh].data_dir` is set and its filesystem lacks room for the database's
  current size plus 2 GB;
- a deploy holds the deploy lock.

It then stops the environment's units: the `[refresh].units` timers first, so
nothing starts behind it, then the other `[refresh].units` services, then the
`[service]` unit. List every unit that touches the database or sends anything
outside: workers, beat or cron units, mail pollers. Before dropping, it reads
the marker and the database's sessions again. Another config's marker, or
any other session still connected (named in the refusal), stops the refresh;
the units that were running start again.

Then it drops the database, re-creates it owned by the app role, marks it
again, and restores the archive. It runs the migrations forward as the app
role, then checks that at least `min_tables` tables came back. A failure from
the drop on leaves the units stopped.

### Who does what

The operator and the app role need not be superusers. With
`[database].admin_url_env`, the admin identity creates the database and the
archive's extensions, restores the archive's objects as the app role
(`pg_restore --no-owner --no-acl --role=<app>`, so the app role owns them),
restores the extensions' own configuration rows, and rebuilds pg_tviews
TVIEWs. A pg_tviews archive needs it. The migrations run with the app DSN only;
the admin DSN never reaches them.

`--no-acl` drops the archive's grants. A grant to a role other than the app
role must be made again after a refresh; the sanitize step is the place for it.

## Sanitize, the outbound flag, and the restart

After the migrations and the table count, the refresh:

1. **Runs the sanitize step** inside the refreshed database, as the app role.
   Its environment is `PATH` plus the app's `PG*` variables (`PGHOST`,
   `PGUSER`, `PGPASSWORD`, `PGDATABASE`, ...), nothing else: no DSN on argv,
   never the admin identity.
   - A `.sql` file is read once and hashed, and those same bytes are piped to
     `psql -X -1 -v ON_ERROR_STOP=1`: one transaction, nothing committed when
     a statement fails.
   - Anything else runs as an executable with no arguments.
   - The step must not be group- or world-writable, and must be owned by root
     or by the user running the refresh; otherwise the refresh refuses before
     anything stops.
   - `sanitize = "none"` skips it; the plan and the receipt carry the
     `sanitize_reason`.
   - A failed step leaves every unit stopped.
2. **Writes the outbound flag** to `/etc/<name>/<environment>.refreshed.env`:
   `<outbound_flag>=1` (default `FRAISIER_REFRESHED=1`), mode 0640, owned by
   the user running the refresh with the `[service]` unit's group. The app can
   read it but neither change it nor delete it: the refresh refuses a
   directory that is group- or world-writable or owned by anyone but root or
   the operator.
3. **Starts the units that were running**, the `[service]` unit first.
4. **Proves the flag** for each started service: its `MainPID`'s
   `/proc/<pid>/environ` must hold `<outbound_flag>=1`. If one has no main
   process, lacks the flag, or its environment cannot be read, the refresh
   stops every unit again and fails. An unreadable environment is never taken
   as a pass.

Your application reads the variable and keeps its outbound integrations (mail,
webhooks, payment calls, mailbox polling) off while it is `1`.

### Every unit loads the flag

Before anything stops, the refresh asks systemd
(`systemctl show -p EnvironmentFiles`) whether each unit loads the flag file.
For a timer it asks the units the timer starts. A unit that does not load it
is refused by name. `fraisier scaffold` generates the line for the app unit:

```ini
EnvironmentFile=-/etc/checkout/staging.refreshed.env
```

It also generates a drop-in for each of `[refresh].units`
(`/etc/systemd/system/<unit>.d/fraisier-refreshed.conf`; a timer's goes to
the service of the same name). Install them with `fraisier scaffold-install`,
then `systemctl daemon-reload`.

### Run it as root

Only root, or the process's own user, may read `/proc/<pid>/environ`, and
the flag file is owned by whoever writes it. Run `fraisier db refresh` as
root, for example `sudo fraisier db refresh --config ... --yes`. Run as
anyone else, the proof cannot read the app's environment and the refresh
fails.

### Clearing the flag

```sh
sudo fraisier db refresh --config /etc/fraisier/checkout-staging.toml --clear-flag
```

This removes the file and records who (`$SUDO_USER`, else `$USER`, and the
uid) and when, on stderr and in the receipt's `flag_cleared`. It never
restarts a unit: the next start loads the environment without the flag.

### The receipt

Every run under the lock writes
`<state_dir>/refreshes/<fraise>-<environment>.json`. It holds:

- the archive's path, stamp, size and mtime;
- the sanitize step (path, kind and SHA-256, or `none` with the reason) and
  its outcome;
- the migrations applied and the table count;
- the flag's variable and file, and the pid that proved it for each unit;
- the start time and duration;
- for a failure, the step and the redacted reason.

It holds no secrets.

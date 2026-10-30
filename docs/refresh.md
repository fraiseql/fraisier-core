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

### Not yet

Sanitizing and the outbound flag are not built yet. Until they are, a refresh
ends with every unit stopped and says so.

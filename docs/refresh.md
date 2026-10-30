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

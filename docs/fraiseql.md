# Deploying FraiseQL with fraisier

## Supported versions

fraisier is proven against **FraiseQL 2.18.0**, the version pinned in
[`tools/fraiseql-version`](../tools/fraiseql-version). The criterion-1 proof
([`scripts/criterion1/`](../scripts/criterion1/README.md)) deploys that server
three times in a row. Each deploy migrates the database and then health-checks
the restarted server. The proof refuses to start when the server binary reports
a version other than the pin.

FraiseQL 2.18 needs **PostgreSQL 18 or newer**. It refuses to boot on an older
server ("FraiseQL requires PostgreSQL 18 or newer"). With the database-readiness
preflight (#92), declare that floor, and the deploy will refuse an older server
before changing anything:

```toml
[database.requires]
postgres_major = 18
```

Other 2.x releases may work, but they are not tested. Moving the pin to a new
release means updating `tools/fraiseql-version`, recompiling
`scripts/criterion1/schema.compiled.json` with that release's `fraiseql-cli`,
and passing criterion-1 again. A schema compiled by one release does not
necessarily load in another: 2.18.0 refuses a 2.4.0 schema with
`unknown field 'lookup_data'`.

## Order: migrate, then restart

fraisier's deploy runs `preflight → fetch → migrate → activate → restart →
health → verify`. The schema is migrated before the new server starts, so the
server boots against the schema it expects.

A 2.18 server started with only the observers capability creates no tables of
its own at boot. The observer tables (`tb_observer`, `core.tb_entity_change_log`
and others) come from fraiseql-observers' migrations. Ship those in your
migration set if you enable observers. fraisier's `verify` step runs your
migrations' `.verify.sql` checks through confiture. It never diffs the whole
schema, so tables the server or its migrations add do not affect it.

## What `/health` tells you

With the `http` health adapter, a deploy passes when `/health` answers the
expected status code. On 2.18 that code is **200 even when a configured
capability failed to start**. Only the body shows the failure: the capability's
`started` is `false`, and the top-level `status` is `degraded`. The real captures
are in
[`crates/fraisier-adapter-http/tests/fixtures/fraiseql-2.18.0/`](../crates/fraisier-adapter-http/tests/fixtures/fraiseql-2.18.0/README.md).

A status-code check therefore proves the server is up, not that every capability
you configured is running. Requiring named capabilities in `verify` is planned.

# fraisier-db

Generic-PostgreSQL database lifecycle operations for [fraisier](../../README.md):

- **backup** — `pg_dump -Fc` to a custom-format archive
- **restore** — `pg_restore` into the target database
- **reset** — drop all user schemas and recreate `public` (the caller then
  re-runs migrations through the migration adapter)

These act at the Postgres level and work regardless of which migration adapter a
deploy uses — *migrations* stay with the adapter (the Confiture model); this
crate owns only the dump/restore/wipe lifecycle.

## Secrets never reach argv

A DSN carries the password, so this crate decomposes it (`PgConn::parse`) and
exposes the parts as libpq `PG*` environment variables (`PgConn::pg_env`). The
command builders set that environment on the child process and pass no
connection string on the command line — only non-secret values (such as the
target database name `pg_restore` requires) ever appear as arguments.

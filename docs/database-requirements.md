# Database requirements: `[database.requires]`

A deployable can declare what its PostgreSQL server must offer. fraisier checks
the declaration and refuses a server that does not meet it. It never installs
PostgreSQL or an extension: that is the operator's job, because it needs root
on the host and superuser in the database.

```toml
[migration]
database_url_env = "CHECKOUT_DATABASE_URL"   # the probe reads this DSN

[database.requires]
postgres_major = 18   # PostgreSQL 18.x or later
pg_tviews = true      # preloaded, installed, and on read contract 2
```

## When it is checked

- **First in every deploy's preflight** (single-host, multi-host and
  blue-green), before the migration adapter is asked anything and before any
  mutation. A server that is not ready rolls the deploy back at `preflight`, and
  the reason names every requirement it fails.
- **`fraisier doctor`**, as the `database_ready` check. This is the only time
  `doctor` connects to the database.

A deliberate rollback (`fraisier rollback`) is not held behind the check:
refusing an emergency rollback protects nothing. `--skip-preflight` skips it
along with the rest of preflight.

## What is checked

The probe runs `psql` with the DSN in `PG*` environment variables, never on
argv, and fails closed: anything it cannot read is a refusal, never a skip.

| Requirement | Refused when | What to do |
|---|---|---|
| `postgres_major` | `server_version_num` is below the floor | upgrade PostgreSQL |
| `pg_tviews` | `pg_tviews` is not in `shared_preload_libraries` | add it and restart PostgreSQL. Without the preload, `CREATE TABLE tv_… AS …` silently makes a plain table |
| | this role cannot read `shared_preload_libraries` | `GRANT pg_read_all_settings TO <role>` |
| | the extension is not installed in the database | a superuser runs `CREATE EXTENSION pg_tviews` |
| | `contract_version()` cannot be called | upgrade pg_tviews (releases before 0.1.0-beta.30 have no contract 2), or grant `USAGE` on its schema |
| | the contract is older than 2 | upgrade pg_tviews |
| | the contract is newer than 2 | upgrade fraisier |

The extension's schema is read from the catalogue, not assumed.
`pg_extension.extversion` is not used, because it reads `0.1.0` on every beta.

## Getting a host ready

[`scripts/provision-pg18-tviews.sh`](../scripts/provision-pg18-tviews.sh) gets a
Debian 12 host ready for `postgres_major = 18` and `pg_tviews = true`. Debian
13 is accepted too, but the acceptance run below covers only 12. Run it as root
on the database host:

```sh
sudo scripts/provision-pg18-tviews.sh --database checkout
```

It does what this page otherwise leaves to the operator:

1. Adds the PGDG apt repository. It refuses a signing key whose fingerprint is
   not PGDG's.
2. Installs `postgresql-18` and the packages a pg_tviews build needs.
3. Builds pg_tviews from a pinned commit, the signed v0.1.0-beta.32 tag, with
   the pgrx version pg_tviews pins. The Rust toolchain for that build lives in
   `/var/lib/fraisier-provision`, apart from any other.
4. Adds `pg_tviews` to `shared_preload_libraries`, keeping what is already
   there, and restarts PostgreSQL. It restarts only when that setting changed,
   or when a preloaded pg_tviews was just rebuilt.
5. Creates the database when it is absent.
6. Runs `CREATE EXTENSION pg_tviews` in it, or `ALTER EXTENSION pg_tviews
   UPDATE` after a rebuild.

Every step checks before it acts. The last line counts what changed, so a second
run prints `0 change(s)` and leaves PostgreSQL running.

The script does not create your app's role or grant it anything. A role that is
not a superuser needs two grants for the probe to read what it checks:

```sql
GRANT pg_read_all_settings TO checkout_app;
GRANT USAGE ON SCHEMA tviews TO checkout_app;  -- the extension's schema
```

`scripts/hetzner-pg18-tviews.sh` is the script's acceptance run, on a
throwaway Debian 12 VPS. It provisions twice and checks that the second run
changes nothing. It runs this page's probe through `fraisier doctor`, and checks
that a `tv_` table registers as a TVIEW. Then it drops the extension, and later
removes the preload, and checks each time that the probe fails with the right
reason until a re-run of the script repairs it.

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

## Empty TVIEWs

A pg_tviews TVIEW can be empty while its backing view has rows. A restore, a
crash-recovery start or a failover can leave an UNLOGGED TVIEW that way, and a
`TRUNCATE` can do it to any TVIEW. Nothing else notices: the API serves empty
results.

With `pg_tviews = true`, every deploy's preflight checks for this right after
the readiness check passes, and so does `doctor`'s `database_ready` check. The
check compares each registered TVIEW with its backing view. It fails closed: a
probe that could not run, or a TVIEW this role cannot read, counts as a finding.

```toml
[database.tviews]
on_empty = "fail"   # the default: refuse the deploy at preflight
# on_empty = "warn" # report it on stderr and continue
```

There is no `off`. A role must be able to read every TVIEW and its view for the
check to clear it; the finding names the `GRANT SELECT` that fixes an unreadable
one.

## The admin identity: `[database].admin_url_env`

Some operations need more than the app role has:

- **Restoring a pg_tviews archive.** `CREATE EXTENSION pg_tviews` needs a
  superuser (the extension is written in C). A non-superuser `pg_restore` loads
  the tables, fails on the extension, and leaves a database with no TVIEW
  machinery.
- **Rebuilding TVIEWs.** pg_tviews does not grant `pg_tviews_rebuild_all` to
  `PUBLIC`.

Name an environment variable that holds a superuser DSN for these, and only
these:

```toml
[database]
admin_url_env = "CHECKOUT_DATABASE_ADMIN_URL"
```

It is used by `fraisier db restore`, `fraisier db tviews rebuild`,
`fraisier backup drill`, and a `restore_rehearsal` preflight. The drill and the
rehearsal create their throwaway database through it (owned by the app role)
and restore into it as the admin. Migrations, health checks and everything else
keep running as the app role. The admin DSN reaches `psql` and `pg_restore`
through `PG*` variables, never argv, and is never printed.

No other child process inherits the variable. Once a config names it, fraisier
removes it from the environment of every command it runs: the migration tool,
hooks, the approval and notify commands, an exec token provider, an IPC
adapter, `systemctl`. A hook that needs a superuser has to be given one some
other way.

On a host set up with `fraisier scheduled install`, put the DSN in
`/etc/<name>/<env>.ops.env`. Every unit that command writes (backup, prune, drill,
a scheduled deploy, and the alert unit) loads that file; the app unit loads only `/etc/<name>/<env>.env`, so the application never
sees the superuser DSN.

**A deploy started by the webhook never uses it.** `fraisier webhook-server`
answers the network, so nothing it starts can reach a database superuser: its
deploys do not even read the variable. Their restore rehearsal creates and
restores the throwaway as the app role. On a pg_tviews database, the rehearsal
refuses before it creates anything, and the refusal says to run the deploy
from the CLI or its timer. A deploy run from the CLI or a timer is unchanged.

The recommended identity is the `postgres` role over the Unix socket, with peer
authentication mapped from the OS user fraisier runs as, so no password exists
to leak:

```text
# pg_ident.conf
fraisier_admin  deploy    postgres
fraisier_admin  postgres  postgres

# pg_hba.conf (replacing the default `local all postgres peer` line)
local  all  postgres  peer  map=fraisier_admin
```

```sh
export CHECKOUT_DATABASE_ADMIN_URL='postgresql://postgres@%2Fvar%2Frun%2Fpostgresql/postgres'
```

The socket directory goes in the host part, percent-encoded. A `?host=` query
parameter is not read.

`fraisier doctor` checks the identity as `database_admin`: it must connect, be a
superuser, and reach the same cluster as the app DSN (the same
`pg_control_system()` system identifier). It must not be the migration DSN's
variable.

### `fraisier db restore` and pg_tviews

Before it changes anything, `db restore` lists the archive. When the archive
creates pg_tviews and the restoring identity is not a superuser, the restore is
refused and the refusal names `admin_url_env`. After a successful restore it
rebuilds the empty TVIEWs, then names any TVIEW still empty over a populated
view. `db restore`, `backup drill` and the rehearsal share this precheck; only
the rehearsal skips the rebuild, because it proves the migrations, not the
TVIEW contents.

## Rebuilding TVIEWs: `fraisier db tviews rebuild`

```sh
fraisier db tviews rebuild --config fraisier.toml         # the empty UNLOGGED ones
fraisier db tviews rebuild --config fraisier.toml --all   # every TVIEW
```

It runs as the admin identity when one is configured, otherwise as the app
role. Either way it first refuses on a standby (TVIEWs can only be rebuilt on
the primary) and without `EXECUTE` on the rebuild function, naming the `GRANT`.

Without `--all`, pg_tviews refills only the UNLOGGED TVIEWs PostgreSQL reset. A
LOGGED TVIEW (the default since pg_tviews 0.1.0-beta.28) that was emptied by
hand stays empty. The command then names what is still empty and exits 1; run it
again with `--all`.

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

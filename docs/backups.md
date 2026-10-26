# Backups and retention

## Taking backups

`fraisier backup` dumps the database with `pg_dump -Fc`. With `[backup].dir`
set, each run writes a new archive into that directory, named
`<fraise>-<environment>-<yyyymmddThhmmssZ>.pgdump` (UTC, to the second):

```toml
[backup]
dir = "/var/backups/checkout"
```

An archive is renamed into place only after `pg_restore --list` reads it back
and it is at least half the size of the newest archive already there. A dump that
fails either check is deleted. A crashed run therefore leaves at most a hidden
`.<name>.partial` file, and retention never counts that file.

## Retention

Each `[[backup.retain]]` entry is a policy for one directory. `fraisier backup
prune` applies every entry. It is a dry run that only lists what it would remove;
pass `--apply` to remove. Pass `--name <entry>` to prune a single entry.

<!-- retain-example -->
```toml
[backup]
dir = "/var/backups/checkout"

[[backup.retain]]
name = "nightly"
dir = "/var/backups/checkout"
schedule = "daily 05:30"
retention_days = 14
keep_minimum = 3
keep_last = 60
match = "checkout-production-*.pgdump"
min_free_gb = 20
```

| Key | Meaning |
|---|---|
| `dir` | The corpus directory. Required. Must be absolute. |
| `schedule` | When the prune runs. Required. Uses the `[schedule].calendar` vocabulary, such as `daily HH:MM` or `hourly`. |
| `retention_days` | Archives older than this are removed. Required, at least 1. |
| `keep_minimum` | The newest this many **valid** archives are kept however old they are. Default 3; 0 means no floor. |
| `keep_last` | At most this many archives are kept, however young the rest are. Optional. At least 1, and not below `keep_minimum`. |
| `match` | A file-name glob (`*`, `?`) choosing the corpus within `dir`. Default `*.pgdump`. It cannot contain `/`. |
| `min_free_gb` | Warn when the volume holding `dir` has less than this much free space. Optional. |
| `name` | Names the entry and its units. Defaults to the last component of `dir`. Must be unique. |

An archive is dated by the timestamp in its name. An archive with no timestamp
in its name is dated by its mtime.

### The floor comes first

The floor is applied before the age rule. If backups stop arriving, every archive
eventually ages past `retention_days`. Without the floor, the whole corpus would
age out together. With it, the newest `keep_minimum` archives survive.

The floor counts archives that `pg_restore --list` can read:

- An archive it rejects gives up its slot, and the next readable archive takes
  it.
- An archive that cannot be checked keeps its slot. This happens when
  `pg_restore` is not installed, or is older than the `pg_dump` that wrote the
  archive. A missing or outdated binary therefore never changes what is
  retained.

Only archives competing for a slot are checked. Pruning a large corpus does not
read every file.

### What makes prune refuse, fail, or warn

A prune **refuses**, exits 1 and removes nothing when:

- the config is invalid;
- the config declares no `[[backup.retain]]` entry;
- `--name` names no entry;
- a deploy of this deployable holds the deploy lock, because the deploy may be
  about to restore from the corpus.

A prune **fails** that entry, and exits 1 after running every other entry, when:

- the entry's `dir` does not exist;
- an archive could not be removed.

A prune **warns** on stderr and still exits 0 when:

- every survivor is older than `retention_days`, so only the floor is keeping
  them;
- an archive is unreadable;
- a matching symlink resolves outside `dir`. Such a link is never removed, and
  neither is what it points to;
- the volume is below `min_free_gb`.

Exiting non-zero here would stop the pruning, which is the one thing that might
still help.

## Restore drills

An archive that has never been restored is not yet a backup. `fraisier backup
drill` restores the newest archive in `[backup].dir` into a throwaway database,
runs your migrations forward on it, counts its tables, and drops it:

```toml
[backup]
dir = "/var/backups/checkout"

[drill]
max_age_hours = 48
min_tables = 20
data_dir = "/var/lib/postgresql"
```

| Key | Meaning |
|---|---|
| `max_age_hours` | Fail when the newest archive is older than this. Default 48, at least 1. A drill on a stale archive proves nothing about tonight's. |
| `min_tables` | Fail when the restored copy holds fewer tables than this. Optional, at least 1. |
| `data_dir` | The Postgres data directory. When set, the drill refuses unless its filesystem has room for another copy of the live database plus 2 GB. Must be absolute. |

The throwaway is created on the live database's server and named
`<database>_fraisier_rh_<pid>_<time>`.

### What makes a drill fail

Before anything is created, a drill fails when:

- the config is invalid, or `[backup].dir` is unset;
- there is no archive of this deployable in the directory;
- the newest archive does not read back with `pg_restore --list`;
- the newest archive is older than `max_age_hours`;
- `data_dir` is set, and its filesystem lacks the room, or the room could not
  be measured.

After restoring, a drill fails when the migrations fail on the copy, or when it
holds fewer than `min_tables` tables. The copy is dropped either way.

### The teardown guard

A drill drops only a database whose name carries `_fraisier_rh_` and differs
from the live database's name. If either check fails, the drill stops before
migrating and leaves the database in place.

If the drop itself fails, the drill reports a warning naming the database.
`fraisier doctor` then lists it among any other leftover `_fraisier_rh_*`
databases. Doctor prints the command that drops each one and never runs it,
because a database it lists may belong to a drill or deploy that is still
running. Doctor connects to the database for this only when the config has a
`[drill]` or `preflight_mode = "restore_rehearsal"`.

### The receipt

Every drill that gets past loading the config writes
`<state-dir>/drills/<fraise>-<environment>.json`, replacing the previous one.
It records:

- the archive and when it was taken;
- `ok`;
- on success, the throwaway's name, the migrations applied, and the table count;
- on failure, the failed step and the error, with any credentials redacted.

## Staging refresh

`fraisier db refresh` replaces this environment's database with the newest
archive another environment's backups wrote, then migrates it forward. It runs
from the staging config:

```toml
[deploy]
name = "checkout"
environment = "staging"

[refresh]
source_dir = "/var/backups/checkout"   # where production's archives land
source_environment = "production"
max_age_hours = 48
min_tables = 20
data_dir = "/var/lib/postgresql"
schedule = "daily 04:30"
```

| Key | Meaning |
|---|---|
| `source_dir` | The directory holding the source environment's archives. Required, absolute. Only `<fraise>-<source_environment>-*` archives are considered. |
| `source_environment` | The environment whose archives are restored. Required, and never this config's own environment: a `[refresh]` pasted into the production config does not load. |
| `max_age_hours` | Fail when the newest archive is older than this. Default 48. |
| `min_tables` | Fail when the refreshed database holds fewer tables than this. Optional. |
| `data_dir` | When set, refuse unless its filesystem has room for the target's current size plus 2 GB. |
| `schedule` | When `scheduled install` runs it. Absent, it runs only when invoked. |

Without `--yes`, `db refresh` shows the plan: the archive it would restore, the
target, and whether the target is marked.

### The marker

A refresh drops a database, so it refuses any database that does not carry
the refresh marker naming this config:

```sh
fraisier db mark-refresh-target --config /etc/fraisier/checkout.staging.toml
```

This sets `fraisier.refresh_target = 'checkout/staging'` on the database
(`ALTER DATABASE … SET`), once. Marking a database already marked for another
config is refused; clearing a marker is a deliberate act, by hand
(`ALTER DATABASE … RESET fraisier.refresh_target`). The refresh sets the
marker again right after it re-creates the database, before restoring into it.
`fraisier doctor` reports it as `refresh_target` when the config has a
`[refresh]`.

The guard fails closed. A database nobody marked is never a target, so an
environment nobody thought to list is still protected. A list of databases a
refresh must not touch would fail open. The marker cannot travel inside an
archive either: `pg_dump` does not carry database-level settings.

### What a refresh does

1. It takes the deploy lock. With `--skip-if-locked`, which the generated unit
   passes, a held lock exits 0 and skips this run. Without the flag, a held lock
   fails the run.
2. It lists the newest source archive with `pg_restore --list`, then checks its
   age and the free space.
3. It checks the marker, the source environment, and every extension the
   archive creates. Each extension must be available on this server, and an
   untrusted one (pg_tviews, for one) needs a superuser.
4. It stops the `[service]`. A service adapter that cannot stop refuses, and a
   config with no `[service]` skips this step.
5. It drops the database, creates it again owned by the connecting role, and
   sets the marker on it.
6. It restores the archive with `--no-owner --no-acl`, because the source
   environment's roles need not exist here. It then migrates up and counts the
   tables.
7. It starts the service.

Nothing changes before step 4. A failed drop restarts the service. Any failure
after the drop leaves the service **stopped**, because the database it would
read is half-restored or unmigrated, and serving from it is worse than serving
nothing. The failure says so.

Every run with `--yes` writes `<state-dir>/refreshes/<fraise>-<environment>.json`.
It records the archive, `ok`, and either the migrations applied and the table
count, or the failed step and the error with credentials redacted.

## Scheduling and failure alerts

`fraisier scheduled install` writes a timer and a oneshot service for each
scheduled run. It does not need a `[schedule]` section, so a host that only
prunes gets its units too:

```toml
[backup]
dir = "/var/backups/checkout"

[[backup.retain]]
dir = "/var/backups/checkout"
name = "nightly"
schedule = "daily 05:30"
retention_days = 14

[drill]
schedule = "weekly Sun 04:00"

[notify]
command = "/usr/local/bin/page-oncall"
```

| Run | Units |
|---|---|
| `[schedule] command = "backup"` | `fraisier-<name>-<env>-scheduled.{service,timer}` |
| each `[[backup.retain]]` entry | `fraisier-<name>-<env>-retain-<entry>.{service,timer}`, running `backup prune --name <entry> --apply` |
| `[drill].schedule` | `fraisier-<name>-<env>-drill.{service,timer}` |
| `[refresh].schedule` | `fraisier-<name>-<env>-refresh.{service,timer}`, running `db refresh --yes --skip-if-locked` |

A retain entry's name becomes part of its unit names, so it may hold only
letters, digits, `_`, `.` and `-`.

### Every failure notifies exactly once

Each of those services sets
`OnFailure=fraisier-<name>-<env>-alert@%n.service`, and `scheduled install`
always installs that alert template with them. An `OnFailure=` that names a
missing unit fails silently.

1. The failing run starts with `--failure-record %n`. When it fails, it writes
   a record to `/run/fraisier/failures/<INVOCATION_ID>.json`, or under
   `$XDG_RUNTIME_DIR` for a user unit. The record holds the event, the fraise
   and environment, the unit, the invocation ID, the time, and the reason with
   credentials removed. It is mode 0600 and at most 4 KiB. The run itself
   notifies nothing.
2. systemd then starts the alert unit. Its `fraisier notify --unit %i` finds the
   record by `$MONITOR_INVOCATION_ID` and redacts the reason again on read.
   It runs `[notify].command` once, then deletes the record.
3. Sometimes there is no record: the run was killed before it could write one,
   or it ran without the flag. The alert still fires, with the reason
   `no failure record; see journalctl _SYSTEMD_INVOCATION_ID=<id>` (or
   `-u <unit>` when the ID is unknown).

`[notify].command` runs via `sh -c`. It receives the payload as JSON on stdin,
and in `FRAISIER_NOTIFY_EVENT`, `_REASON`, `_FAILED` (`<fraise>/<environment>`),
`_FRAISE`, `_ENVIRONMENT`, `_UNIT` and `_INVOCATION_ID`. The events are:

| Event | Run |
|---|---|
| `scheduled-backup-failed` | a scheduled backup |
| `scheduled-prune-failed` | a prune |
| `scheduled-drill-failed` | a drill |
| `scheduled-refresh-failed` | a staging refresh |

With no `[notify].command`, the alert writes the payload to stderr, and the
journal keeps it.

A scheduled **deploy** is not on this path. It notifies in-process when it rolls
back (`scheduled-deploy-failed`, or `policy-blocked`), and its unit has no
`OnFailure=`, so nothing is sent twice. `[notify].command` is its required sink;
the old `[schedule].notify` key is refused with a pointer to it.

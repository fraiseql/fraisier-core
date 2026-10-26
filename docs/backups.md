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

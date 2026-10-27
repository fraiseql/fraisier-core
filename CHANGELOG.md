# Changelog

All notable changes to fraisier are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **`backup drill` restores a pg_tviews archive.** It now restores as
  `[database].admin_url_env` when that is set, then rebuilds the TVIEWs the
  restore left empty. The receipt records the rebuild under `tviews_rebuilt`.
  Without the admin identity, the drill refuses such an archive before it
  creates anything, and the refusal names the setting. A TVIEW that is still
  empty over a populated view after the rebuild fails the drill.
- **A deploy started by the webhook never uses `[database].admin_url_env`.**
  The webhook answers the network, so nothing it starts may reach a database
  superuser; its deploys do not even read the variable. Their restore rehearsal
  runs as the app role. On a pg_tviews database, the rehearsal refuses before it
  creates anything, and says to run the deploy from the CLI or its timer. CLI
  and timer deploys are unchanged.
- **The admin DSN stays out of the app and of every child process.** The
  scheduled units that `fraisier scheduled install` writes (backup, prune,
  drill, a scheduled deploy, and the alert unit) now also load
  `/etc/<name>/<env>.ops.env`, after the shared `<env>.env`; the app unit does
  not load it. Put `[database].admin_url_env`'s DSN there, and the drill can
  restore as the admin while the application never sees it. Once a config
  names `admin_url_env`, fraisier also removes that variable from every
  command it runs: the migration tool, hooks, the approval and notify
  commands, an exec token provider, IPC adapters and `systemctl`. Only the
  restore tools get the DSN, as `PG*` variables. Re-run `scheduled install` to
  pick up the new units.
- **Every restore goes through one primitive in `fraisier-db`.** `db restore`,
  the restore rehearsal and the drill share one identity choice, archive
  listing, pg_tviews precheck, rebuild and teardown guard.
  - **The rehearsal** now refuses a pg_tviews archive it cannot restore before
    it creates the throwaway. Before, `pg_restore` failed halfway. It still does
    not rebuild TVIEWs: it proves the migrations, not the TVIEW contents.
  - **`db restore`** now reports any TVIEW still empty over a populated view
    after its rebuild, and points at `fraisier db tviews rebuild --all`.

- **Breaking: the failure sink moved from `[schedule].notify` to
  `[notify].command`.** A host that only prunes has no `[schedule]`, yet its
  failures need somewhere to go, so the sink now belongs to the deployable. The
  old key is refused with a validation error that names the new one. There is
  no alias: two spellings of one sink would drift.

- **The Rust toolchain is pinned to an exact release, 1.99.0, and the MSRV is
  now 1.99** (it was a declared 1.95 that nothing ever built with). A floating
  `channel = "stable"` let Rust 1.99's new clippy lints turn an untouched `main`
  red; an exact pin moves only in a commit that carries its own lint fixes. The
  MSRV is the pinned version because that is the only version the gate tests. A
  new gate step, `cargo xtask toolchain`, refuses a channel that is not an exact
  `X.Y.Z` and a `rust-version` that is not its `X.Y`, so the two move together.
  `clippy.toml`'s separate `msrv` is gone (clippy reads `rust-version`). Closes
  [#80](https://github.com/fraiseql/fraisier-core/issues/80).

- **The CI gate now runs rustdoc**, as `cargo xtask doc`
  (`RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --all-features`).
  Nothing ran rustdoc before, which is how nine broken doc links accumulated
  across six crates unnoticed — a broken intra-doc link is invisible while
  writing and ships as a dead link on docs.rs, the only documentation an
  embedder of these crates reads. All nine are fixed: six redundant explicit
  link targets, a hyphenated crate name that was never a valid path
  (`fraisier-core`), and two links to the saga's feature-gated `otel` module.
  Those last two are now code spans rather than links: these crates set no
  docs.rs `all-features` metadata, so docs.rs builds them with default features
  and a feature-gated path cannot be a working intra-doc link there.

### Added

- **Stacks: `fraisier stack deploy` and `fraisier stack status`.** A
  `stack.<env>.toml` references each member's own config by path, with
  `depends_on`; a member's name is its `[deploy].name`, and every member must
  deploy to the stack's environment. `stack deploy` deploys the members in
  dependency order (ties by name), each as `fraisier deploy` would, and stops at
  the first failure; there is no rollback across members. `stack status` shows
  each member's health, version and last deploy, and one row per database the
  members declare `[database.requires]` for; it exits 1 when anything is
  unhealthy. With `[stack].router`, both check each subgraph's live
  `{ _service { sdl } }` against the SDL file in the router's release and fail
  on a difference. See `docs/stacks.md`. Part of
  [#85](https://github.com/fraiseql/fraisier-core/issues/85).

- **`[migration] adapter = "none"`: a deployable that owns no database.** A
  router or a static front end used to fill the migration axis with no-op
  commands, and a no-op whose `current_revision` printed nothing turned every
  failed deploy into `partial_rollback`, because migrate cannot undo an
  "initial" migration. Declaring `none` tells the saga there is nothing to
  compensate on that axis, so the deploy ends `rolled_back`. The adapter takes
  no `database_url_env` and no `[migration.settings]`; either one is a
  validation error.

- **`deploy --dry-run --deep`: the deep classification is previewed only on
  request.** Under a fresh-dump restore rehearsal, a plain `--dry-run`
  classifies statically and says so; it never dumps the live database.
  `--deep` previews the classification the deploy will make, against a
  throwaway restored from a fresh dump. See
  [docs/schema-risk-policy.md](docs/schema-risk-policy.md#deep-classification-under-restore-rehearsal).

- **A failed scheduled backup, prune or drill notifies exactly once.**
  - **The units.** `fraisier scheduled install` now also writes a prune timer per
    `[[backup.retain]]` entry, and a drill timer when `[drill].schedule` is set.
    It no longer needs a `[schedule]` section to do so. Each of those services,
    and a scheduled backup's, sets
    `OnFailure=fraisier-<name>-<env>-alert@%n.service`. The alert template is
    always installed alongside.
  - **The record.** The failing run (`--failure-record %n`) leaves a redacted
    record keyed by its invocation ID: mode 0600, at most 4 KiB, under
    `/run/fraisier/failures`. The run itself notifies nothing.
  - **The alert.** `fraisier notify --unit` finds the record by
    `$MONITOR_INVOCATION_ID`, sends it once through `[notify].command`, and
    deletes it. Without a record, it still notifies, with a journalctl pointer.
  - **Events.** `scheduled-backup-failed`, `scheduled-prune-failed` and
    `scheduled-drill-failed`.
  - **Deploys.** A scheduled deploy keeps notifying in-process and gets no
    `OnFailure=`.
  - **Docs.** See [docs/backups.md](docs/backups.md#scheduling-and-failure-alerts).

- **`[metrics].textfile_dir`: deploy, backup and drill write Prometheus textfile
  metrics.**
  - **One file per command.** The files are `fraisier_deploy.prom`,
    `fraisier_backup.prom` and `fraisier_drill.prom`, for node_exporter's
    textfile collector. Each is renamed into place, so a scrape never reads half
    a file, and a command never overwrites another command's file.
  - **Gauges of the last run.** Deploy records its finish time, whether it
    committed, whether it rolled back (fully or partly), and how long it took.
    Backup records its success time and archive size. Drill records its
    success time. A failed backup or drill leaves its file alone, so the last
    success time stays true.
  - **Labels.** Every sample carries exactly `fraise` and `environment`.
  - **Failures.** A file that cannot be written is a warning, never a failed
    command.
  - **Docs.** See [docs/metrics.md](docs/metrics.md).

- **`fraisier backup drill`: the newest archive is proven to restore.**
  - **What it does.** It restores the newest archive in `[backup].dir` into a
    throwaway database, runs the migrations forward on it, counts its tables,
    and drops it. The drop runs whether the drill passed or failed.
  - **Checks before anything is created.** The drill fails when there is no
    archive, when the newest one does not read back with `pg_restore --list`,
    or when it is older than `[drill].max_age_hours` (default 48). It also
    fails when the filesystem holding `[drill].data_dir` cannot take another
    copy of the live database plus 2 GB. A failed measurement refuses too.
  - **Checks on the restored copy.** The drill fails when the migrations fail,
    or when fewer than `[drill].min_tables` tables come back.
  - **The teardown guard.** The throwaway is dropped only if its name carries
    `_fraisier_rh_` and differs from the live database. Otherwise the drill
    neither migrates nor drops it.
  - **The receipt.** Every run writes a redacted JSON receipt to
    `<state-dir>/drills/<fraise>-<environment>.json`, atomically.
  - **Doctor.** `fraisier doctor` lists leftover `_fraisier_rh_*` databases and
    prints the `DROP DATABASE` command for each. It never drops them. It
    connects to the database for this only when the config has a `[drill]` or
    `preflight_mode = "restore_rehearsal"`.
  - **Docs.** See [docs/backups.md](docs/backups.md#restore-drills).

- **`[[backup.retain]]` and `fraisier backup prune`: archives are pruned by
  policy, never below a floor of valid ones.**
  - **The rules.** Each entry names a `dir` and a `schedule`, plus these keys:
    - `retention_days`: older archives are removed;
    - `keep_minimum`, default 3: the newest that many valid archives are always
      kept;
    - an optional `keep_last` cap: at most that many are kept, however young.
      It is rejected when below the floor.
  - **Only archives can be selected.** A prune removes every old file its
    entry selects, so `dir` must be `[backup].dir` or below it (making
    `[backup].dir` required once an entry exists), and `match` must end in
    `.pgdump`. `match = "*"` or a `dir` outside the backups is a validation
    error.
  - **The floor comes first.** It is applied before the age rule, so a producer
    that has stopped never ages the whole corpus out. An archive `pg_restore
    --list` rejects gives up its floor slot. One that cannot be checked keeps
    it, including when `pg_restore` is older than the `pg_dump` that wrote it.
    Python fraisier counted that case as invalid, so a version skew could remove
    good archives.
  - **Refusals (exit 1).** The prune is a dry run unless `--apply` is given. It
    refuses while a deploy holds the deploy lock, and with no policy or an
    unknown `--name`. A missing `dir` fails the prune.
  - **Warnings (stderr, exit 0).** These cases warn: only the floor is keeping
    the corpus; an archive is unreadable; the volume is below `min_free_gb`; a
    matching symlink resolves outside `dir`. Such a symlink is never removed.
  - **Docs.** See [docs/backups.md](docs/backups.md).
  - **Library.** `fraisier_db::retention` exposes the selection.
    `fraisier_db::archive::verify_archive` is the `pg_restore --list` check that
    both backup and prune use.
  - **Config.** `[backup]` now needs `dir` or at least one retain entry.

- **Empty pg_tviews TVIEWs refuse a deploy, and `fraisier db tviews rebuild`
  refills them.** With `[database.requires] pg_tviews = true`, preflight and
  `doctor` check every TVIEW against its backing view, after readiness passes.
  `[database.tviews] on_empty = "fail"` (the default) refuses the deploy, and
  `"warn"` reports it and continues. The check fails closed. `db tviews rebuild`
  refills the empty UNLOGGED TVIEWs, or every TVIEW with `--all`. It refuses on a
  standby and without `EXECUTE`, and exits 1 when a TVIEW stays empty.
  Part of [#88](https://github.com/fraiseql/fraisier-core/issues/88).

- **`[database].admin_url_env`: a superuser identity for restore-class
  operations only.** `db restore`, `db tviews rebuild` and a `restore_rehearsal`
  preflight use it; nothing else does. `db restore` now refuses a pg_tviews
  archive before changing anything when the restoring identity is not a
  superuser (a non-superuser restore loads the tables and silently drops the
  extension), and rebuilds the TVIEWs after a restore. The rehearsal creates its
  throwaway through the admin, owned by the app role. `doctor` checks the
  identity (`database_admin`): it connects, is a superuser, and reaches the app
  DSN's cluster. Peer authentication over the Unix socket is the documented
  setup; see `docs/database-requirements.md`.
  Part of [#83](https://github.com/fraiseql/fraisier-core/issues/83).

- **`scripts/provision-pg18-tviews.sh`: a documented way to get a Debian host
  ready for `[database.requires] postgres_major = 18, pg_tviews = true`.**
  - **What it does.** It installs PostgreSQL 18 from PGDG, after checking the
    archive key's fingerprint. It builds pg_tviews at a pinned commit
    (v0.1.0-beta.32) with the pgrx it pins, adds it to
    `shared_preload_libraries`, and restarts PostgreSQL only when that changed.
    It creates the database and the extension when they are absent.
  - **Idempotent.** Every step checks before acting, and the last line counts
    the changes. A second run reports `0 change(s)` and does not restart
    PostgreSQL.
  - **Acceptance.** `scripts/hetzner-pg18-tviews.sh` runs on a fresh Debian 12
    VPS. It provisions twice, runs fraisier's `database_ready` check, proves a
    TVIEW registers, and checks that dropping the extension and removing the
    preload each fail the check with the right reason until a re-run repairs
    it.
  - **Docs.** See `docs/database-requirements.md`.

- **`[database.requires]`: declare what the database server must offer, and
  fraisier refuses a server that does not.** `postgres_major` sets a PostgreSQL
  major-version floor, and `pg_tviews = true` requires pg_tviews to be in
  `shared_preload_libraries`, installed, and on read contract 2. The check runs
  first in every deploy's preflight (single-host, multi-host and blue-green) and
  in `fraisier doctor` (`database_ready`). It fails closed, and each refusal
  names the expected and actual value and the fix. fraisier never installs
  either; see `docs/database-requirements.md`. Without the section nothing
  changes. Part of [#83](https://github.com/fraiseql/fraisier-core/issues/83).

- **FraiseQL 2.18.0 is the supported, proven version, pinned in
  `tools/fraiseql-version`.**
  - **The proof.** Criterion-1 now deploys a 2.18.0 `fraiseql-server` three
    times in a row. Every criterion-1 script reads the pin, and refuses a
    server whose `--version` differs from it.
  - **The boot schema** is recompiled with 2.18.0, which refuses the old 2.4.0
    one.
  - **`deploy-local.sh`** now:
    - recreates a scratch database named `fraisier_criterion1_*`;
    - reaches Postgres through `PG*` variables instead of a DSN on `psql`'s
      argv;
    - takes the server and fraisier binaries from the environment.
  - **The Hetzner orchestrator** ships this repository's assets and the pin. It
    no longer ships hand-copied ones.
  - **Real captures.** These sit in
    `crates/fraisier-adapter-http/tests/fixtures/fraiseql-2.18.0/`, with a test
    pinning what they show. They are 2.18's `/health` bodies for a capability
    that is running, one configured but failed to start, and one not configured.
    Two facts matter:
    - `/health` answers 200 even when a configured capability failed to start;
    - the observers runtime never reports `serving`.
  - **Docs.** See [docs/fraiseql.md](docs/fraiseql.md).

- **Releases carry a `SHA256SUMS` file and a CycloneDX SBOM per target.**
  `release-binaries.yml` now attaches `fraisier-<target>.cdx.json` next to each
  tarball, and a final job hashes every tarball and SBOM *as the release serves
  them* into `SHA256SUMS`. If any target fails to build, no `SHA256SUMS` is
  written, so it can never silently omit a binary. Check a download with
  `sha256sum --check --ignore-missing SHA256SUMS`. Part of
  [#81](https://github.com/fraiseql/fraisier-core/issues/81).
- **`SECURITY.md`**: private vulnerability reporting through GitHub, and the
  supported-version policy (the latest beta only, until 1.0.0).
- **`docs/security.md` states that the binary makes no call-home**, and lists
  every outbound connection with the setting that turns it on. It also gains an
  offline install procedure.

- **`[backup].dir`: each backup writes a new, timestamped archive into a
  directory**, named `<fraise>-<environment>-<yyyymmddThhmmssZ>.pgdump` (UTC, to
  the second). `pg_dump` writes into a hidden `.<name>.partial` file beside it,
  and the archive is renamed into place only after `pg_dump` exits 0,
  `pg_restore --list` reads it back, and it is at least half the size of the
  newest archive already there (a smaller one looks like a truncated dump). A
  dump that fails any check is deleted, so it can neither replace a good archive
  nor become the next run's size baseline. Two runs in the same second refuse
  rather than overwrite. If `pg_restore` cannot be run, the archive is kept with
  a warning. `--output` still writes exactly where it says, and without
  `[backup].dir` the command behaves as before. `fraisier_db::archive` lists the
  archives and picks the latest by the timestamp in the name, never by mtime.

### Fixed

- **`preflight_mode = "off"` now turns the lint off on multi-host deploys and
  `db migrate` too.** Both read only the legacy `forward_compatible_lint` flag,
  so `"off"` still linted there while `deploy --dry-run` said preflight was
  off. `preflight_mode = "restore_rehearsal"` beside `[hosts]` now warns that
  the multi-host rollout lints live instead of rehearsing.
||||||| eac43bc
- **`stack deploy` validates every member before deploying any.** A member's
  config was validated only when its own deploy began, so an invalid third
  member refused after the first two were already live. The stack is now
  refused up front, naming each invalid member and its errors.
||||||| eac43bc
- **A notify hook that fails is no longer reported as sent.** `fraisier
  notify` (the `OnFailure=` alert unit) printed "sent" and exited 0 whatever
  `[notify].command` did, and the hook's exit status went only to a log line
  the CLI never prints. It now exits 1 and names the status, so the alert unit
  fails visibly. A deploy whose failure notification did not deliver says so
  in a `warning:` line and a `notify_warning` JSON field.
  `ExecHookNotifier::deliver` returns the outcome; `Notifier::notify` stays
  best-effort.

- **A restore rehearsal that never ran the migrations no longer blames them.**
  Every rehearsal failure used to end in "The pending migrations did not apply
  cleanly on a fresh restore. Fix the migrations". That included a throwaway
  copy that could not be created, such as an app role without `CREATEDB`, and a
  `DATABASE_URL` that did not resolve. Neither applies a single migration.
  `RehearsalError::remedy` now gives each failure its own advice. A provisioning
  failure names `CREATEDB` and `[database].admin_url_env`. Only a failed
  rehearsed `migrate up` points at the migrations. The `--skip-preflight` escape
  is unchanged.

- **The vendored confiture exit-code contract is confiture's again, and a test keeps
  it so.** `crates/fraisier-adapter-confiture/src/exit_codes.vendored.json` is meant
  to be `confiture --exit-codes-json` captured verbatim. Measured against confiture
  1.19.0, it was stale in eight of its nine exit entries: every `symbolic_codes` list
  but exit 2's, and the `meaning` of exit 1 ("ambiguous-change advisory") and exit 6
  ("lock or connection-pool contention"). Exit 1 claimed eight codes where confiture
  lists `SQL_001` alone, exit 6 claimed the two `POOL_*` codes confiture does not put
  there, and exits 3, 4 and 5 were missing the codes confiture has added since. The
  two `ExitClass` doc comments repeated the stale wording, and the test matrix used
  `MIGR_105` as an example after confiture had dropped that code entirely. No
  behaviour changes: `classify` keys on the exit integer and nothing dispatches on a
  symbolic code. The file is regenerated and the comments now match it.

  Both guards had stayed green. The always-on one reduces the document to
  `{exit_int: class}`, which never drifted; the live one compared that same reduced map
  and returned a printed `skip:` whenever confiture was absent or too old, and CI
  installed no confiture, so it had never run. It is replaced by a test that compares
  the whole document and fails, rather than skipping, when confiture is missing or is
  not the pinned release — with the drifted fields named one per line. The pin is
  `tools/confiture-requirements.txt` (exact and transitive, on confiture's own 3.14
  floor), and the CI gate installs it and puts it on `PATH` before `cargo xtask ci` — on
  `PATH` rather than in `FRAISIER_CONFITURE_BIN`, because an ambient `FRAISIER_*` variable
  fails the approval hook test and poisons six more
  ([#64](https://github.com/fraiseql/fraisier-core/issues/64)).

  The pin is confiture **1.33.0**, the release fraisier pins. Since 1.19.0 exit 0 has
  gained `DIFFER_404` and exit 5 `CONFIG_014`, `DIFFER_403` and `VALID_003`–`VALID_009`;
  no exit integer changed class, so `classify` and the `AdapterErrorKind` projection are
  unchanged. Closes [#63](https://github.com/fraiseql/fraisier-core/issues/63).

- **The exit-code contract follows confiture 1.35.0.** The pin moves from 1.33.0, and
  the regenerated file adds five codes and changes nothing else. Exit 0 gains
  `DIFFER_405` (an index on a table neither tree declares), `DIFFER_407` (an index on
  a TVIEW) and `DIFFER_408` (an index written before its relation). Exit 5 gains
  `CONFIG_015` (`preflight --scope pending` finds the ledger only in another schema)
  and `DIFFER_406` (an index on a relation the desired tree does not declare, which a
  migration would drop). No exit integer changes class.

- **The exit-code contract follows confiture 1.36.0.** The pin moves from 1.35.0. A
  failed migration, a duplicate version and an embedded `COMMIT` (`MIGR_001`,
  `MIGR_004`, `MIGR_100`, `MIGR_102`, `MIGR_106`–`MIGR_108`) and `GEN_001` move from
  exit 3 to exit 1, so exit 3 (`db_unreachable`) now means `CONFIG_006` alone
  ([fraiseql/confiture#727](https://github.com/fraiseql/confiture/issues/727)). Exit 0
  loses `DIFFER_407`, retired because a TVIEW's indexes are now compared. Exit 5
  gains `DIFFER_409` (a definition change with no statement) and `DIFFER_410` (a tree
  index under a name pg_tviews owns). No exit integer changes class and only
  `LockContention` is retriable, so `classify` and the `AdapterErrorKind` projection
  are unchanged: those migration failures read as `InternalError` instead of
  `DbUnreachable`. The re-frozen transitive set also moves `pydantic` to 2.14.1 and
  `pydantic-core` to 2.50.1.

- **The policy gate judges only the migrations a deploy will apply.** The
  confiture adapter passed no `--scope`, so confiture judged every migration
  file, applied ones included: one approved and applied `DROP COLUMN` refused
  every later deploy. With **confiture ≥ 1.35.0** the adapter now passes
  `--scope pending` and advertises `pending_scope`. Older releases reject the
  flag, so they keep judging every migration; `deploy --dry-run` then says so
  (`scope = "all"` in the JSON), and `fraisier doctor` now warns below 1.35.0.

- **A scheduled backup no longer fails from its second run on.** The generated
  unit runs `fraisier backup --config …` from `/`, which wrote
  `/<fraise>-<environment>.pgdump` once and then refused to overwrite it on every
  later run (and `--force` would have let a failed dump truncate the only good
  archive). A `[schedule]` with `command = "backup"` now **requires** an
  absolute `[backup].dir`: `fraisier validate-config` reports `backup.dir`, and
  `scheduled install` refuses to generate the unit without it. Add a `[backup]`
  section with `dir` to an existing scheduled-backup config before upgrading.
  Closes [#87](https://github.com/fraiseql/fraisier-core/issues/87).

- **`cargo deny` passes again**: `rustls` 0.23.46, `rustls-webpki` 0.103.16 and
  `webpki-root-certs` 1.0.11 clear RUSTSEC-2026-0346 to -0349 and the two
  distrusted-intermediate advisories, which failed the supply-chain gate on every
  branch. `Cargo.lock` only.

- **The test suite no longer fails because of a variable the environment happens
  to carry.** `cargo test` went from 158 passing to 151 whenever any `FRAISIER_*`
  variable was set — including `FRAISIER_CONFITURE_BIN`, which the confiture
  adapter's own README tells people to set. The approval-hook test asserted on the
  child process's *entire* inherited `FRAISIER_*` set, which is a claim about the
  machine rather than about fraisier; it now measures what fraisier *adds*, and
  exports an ambient variable itself so the difference is exercised on every run.
  Separately, a panic under the shared environment lock poisoned it and took six
  unrelated tests down with `PoisonError`, reporting one real failure as seven
  that named neither the cause nor the test; the guard is now recovered, in all
  four crates that keep such a lock. No library or CLI behaviour changes. Closes
  [#64](https://github.com/fraiseql/fraisier-core/issues/64).

## [1.0.0-beta.11] - 2026-09-26

### Security

- **`fraisier-self-upgrade` no longer hands a library consumer an artifact URL's
  credentials, and its notify sink redacts for every producer.** Closes
  [#68](https://github.com/fraiseql/fraisier-core/issues/68).

  `--source` and `--checksum-url` point at an artifact server, and a private one
  reached with basic auth carries its credentials in the URL. A non-success
  response built its error by interpolating that URL verbatim, and the text
  becomes `ApplyOutcome::AbortedBeforeSwap`'s reason. Through the `fraisier`
  binary that was already covered by the output edge added in beta.10, but
  `fraisier-self-upgrade` is a published crate: an embedder calling `apply()`
  reads the reason directly, with no such edge in the way. Every error `download`
  builds is now redacted before it becomes an `Error::Fetch`.

  Redaction also moved *into* the notify sink. `FailurePayload` and the notifiers
  are public, so placing the guarantee on the producer made it hold for exactly
  one producer. `emit_event`'s log line, the hook's `FRAISIER_NOTIFY_REASON` and
  the JSON on the hook's stdin are now all redacted at the sink, from one
  redacted copy, so the two hook channels cannot disagree.

## [1.0.0-beta.10] - 2026-09-26

### Security

- **Credentials no longer ride a failure reason off the host.** Adapters build
  their errors out of whatever their tool wrote to stderr, and a database client
  that cannot connect prints the DSN it tried — password included. That text
  became the saga's rollback reason, and the reason was printed, logged,
  persisted and sent to the failure webhook unchanged. Closes
  [#62](https://github.com/fraiseql/fraisier-core/issues/62).

  Redaction now happens at five boundaries rather than at the places a reason is
  built, so it covers every present and future contributor to one:

  - the CLI's output edge, which is every command's text, its `--json` payload
    and its `--verbose` stderr JSON — so `deploy`, `rollback`, `status`, `list`,
    `health`, `bootstrap`, `provider-test` and the `db` operations are all
    covered, including `status`/`list`, whose `Debug` rendering of a
    `PartialRollback` state showed a payload the `Display` one omits;
  - the `[schedule].notify` payload, which reaches the hook as
    `FRAISIER_NOTIFY_REASON`, as JSON on its stdin, and as the notifier's own
    log event — that event fires whether or not a hook is configured;
  - the `PartialRollback` reason the engine persists, before it is written to
    `state.json`, appended to `events.jsonl`, or pushed to a remote ledger by
    `sync push`;
  - the IPC client's fold of a remote adapter's stderr into the error message;
  - a rendered anyhow chain, which is both the CLI's `error:` line and the
    webhook server's HTTP 500 body — that body leaves the host over the network
    and never passes the output edge.

  Stripped are a URL's `user:password@` and libpq's `password=` keyword form.
  What an operator needs in order to act is kept: which host could not be
  reached, which step failed, and what the tool said.

  `fraisier_saga::redact::credentials` is the single implementation, re-exported
  as `fraisier_core::redact`.

### Changed

- A failed post-migration verify now names its failing checks in the rollback
  reason, and so in the failure webhook, each with the adapter's detail when it
  gave one, for example
  `post-migration verify failed 1 check(s): row count (expected 3 rows, found 0)`
  where the reason used to stop at the count. A `command` migration adapter's
  `verify` therefore carries its captured stderr (or stdout) into the reason.
  The gate itself is unchanged and still decides on `ok` alone.

## [1.0.0-beta.9] - 2026-09-19

### Fixed

- **`verify` no longer passes a run that verified nothing.** The confiture
  adapter read `migrate verify`'s `failed_count == 0` as success, but a
  database with no migration ledger reports a failure count of zero for the
  trivial reason that nothing ran. confiture 1.12.0 states the verdict itself,
  as `ok` beside `was_skipped` (fraiseql/confiture#311). The adapter now reads
  `ok`, and against an earlier confiture falls back to the counts with
  `ledger_present: false` counted as not-ok. Closes
  [#58](https://github.com/fraiseql/fraisier-core/issues/58).

  Green now needs every signal in the payload to agree: `ok`, the failure
  count, each result's status, the ledger, and a clean exit. Two more
  fail-open paths are closed. A payload with no verdict field at all read as
  zero failures, and a clean exit that wrote no report read as a pass. Both
  are now adapter errors that name what was missing.

  **Reachability.** fraisier never passes `--allow-uninitialized`, so the
  ledger-less payload could not reach a deploy through the adapter's own
  invocation. Without the flag, confiture exits 2 and the adapter already
  reported an error. The fail-open defaults were reachable through any
  producer drift. No version floor changes.

### Changed

- **BREAKING (adapter authors): `VerifyReport` is now `#[non_exhaustive]`, and
  carries `was_skipped`.** A verify that examined nothing and one whose checks
  failed used to reach the deploy as the same bare `ok: false`. The report now
  says which. The marker is the same deliberate break `PreflightReport` took in
  beta.7, so that every later field is additive. An **in-process** adapter,
  one that links `fraisier-core` and returns this struct directly as the
  bundled `confiture` and `command` adapters do, moves to the builder:

  ```rust
  // before
  VerifyReport { ok, checks }
  // after
  VerifyReport::new(ok).with_checks(checks)
  ```

  **Out-of-process (IPC) adapters are unaffected.** On the wire `was_skipped`
  is optional and defaults to `false`, and the protocol version does not
  change.

- A failed post-migration verify now says what happened in the rollback reason,
  and so in the failure webhook. A run that examined nothing reads "verified
  nothing" instead of "failed 0 check(s)". The gate itself is unchanged and
  still decides on `ok` alone.

### Added

- `VerifyReport::was_skipped`, and an optional `was_skipped` on the IPC
  `verify` result (`crates/fraisier-ipc/PROTOCOL.md`).

## [1.0.0-beta.8] - 2026-08-08

### Added

- **The schema risk policy gate now classifies.** `[policy]` has been able to
  turn a per-change risk tier into a deploy decision for several releases, but
  no installable confiture emitted a change-set, so fraisier withheld the
  `risk_tier` capability from every version and a configured `[policy]` refused
  every deploy on the grounds that nothing had classified them. The confiture
  adapter now advertises `risk_tier` against **confiture ≥ 0.44.0**, which
  closes [#44](https://github.com/fraiseql/fraisier-core/issues/44) and
  [#46](https://github.com/fraiseql/fraisier-core/issues/46).

  **The operator-visible change:** with confiture ≥ 0.44.0 installed, a
  `[policy]` section that classified nothing now classifies. Tiers you listed
  under `auto_apply` apply; the rest go to the approval hook or refuse, as
  configured. Deploys with no `[policy]` section are unaffected, and so are
  deploys against an older confiture — below the floor the capability stays
  withheld, and `fraisier doctor` names the version and the floor.

  The floor is 0.44.0 and not 0.43.0, where the change-set itself shipped:
  0.43.0 is also the last release whose classifier reports `window_safe: true`
  for a `DROP TABLE` (fraiseql/confiture#206), and a classifier that cannot be
  trusted about the window is not trusted about the tier. Both version-gated
  capabilities now light up on the same release.

  One refusal survives the upgrade by design: an `ALTER COLUMN … TYPE` arrives
  **unclassified** and is therefore refused. Confiture will not guess whether a
  type change widens or narrows without the column's current type, which it
  gathers only when comparing against a live database — something fraisier does
  not ask it to do. Approve it through the hook, or split it into expand →
  backfill → contract. This is documented in `docs/schema-risk-policy.md`.

  `fraisier deploy --dry-run` renders the change-set worst-first and carries it
  in `--json`; the serialized form reports `null` for an unclassified change and
  never invents a tier for it.

### Fixed

- **The IPC spawn no longer fails a call on a transient `ETXTBSY`.** Linux
  refuses to `exec` a file any process still holds open for writing, so
  spawning an adapter can lose a race against a writer that is about to close.
  `fraisier-ipc` had no retry, alone among fraisier's spawn paths —
  `fraisier-adapter-support` has retried its shell-out spawns for as long as it
  has existed — so the IPC client turned that race into a failed call. It now
  retries the same way: five extra attempts, 20 ms apart.

  Retrying is safe because `ETXTBSY` is the one spawn failure that is *always*
  transient — the writer will close. It cannot mask a real misconfiguration: a
  missing binary reports `NotFound` and a non-executable one `PermissionDenied`,
  and neither is retried. A binary that is genuinely held busy still fails, with
  the same error as before, after at most ~100 ms.

  **Where this was observed:** in fraisier's own test suite, where parallel
  async tests each write a fake adapter and spawn it while sibling threads fork.
  It is not a production bug report. The production window is much narrower and
  needs an *external* writer — nothing fraisier does reaches it: every binary
  the IPC client spawns (`fraisier-adapter-<name>` on `PATH`,
  `[artifact].adapter_bin`, `ssh`) is installed out of band, and
  `fraisier self-upgrade` never `exec`s the binary it swaps — by design, it
  hands the restart to the supervisor. What remains reachable is a
  config-management or package run rewriting an adapter binary at the moment a
  deploy spawns it, which this now rides out.

## [1.0.0-beta.7] - 2026-08-06

### Added

- **A risk-keyed policy engine, and the `[policy]` section that drives it.**
  `fraisier_core::policy::evaluate` is one pure decision function over one
  preflight report: it auto-applies the tiers a policy trusts, sends the rest to
  an approval hook, and **denies anything it cannot classify**. The `[policy]`
  section configures it:

  ```toml
  [policy]
  auto_apply = ["additive", "reversible"]
  require_approval = ["lock_risky", "destructive", "irreversible"]
  unclassified = "deny"              # or "require_approval"
  approval_command = "scripts/deploy/approve.sh"
  approval_timeout_secs = 300
  ```

  The section is flat because fraisier's config is already one file per
  environment, so per-environment thresholds are satisfied by construction. Its
  *presence* is the opt-in: with no `[policy]` section the tier gate does not run
  at all and every existing deploy behaves byte-for-byte as before. Nothing calls
  the engine yet — wiring it into the deploy flow, running the approval hook, and
  auditing the decision ship separately — so this release adds the decision and
  its configuration, not a behaviour change.

  Deny-by-default is the law, and it is spelled out in the types rather than left
  to a validation rule: a tier listed in **neither** list is denied (a tier added
  to the taxonomy later cannot silently auto-apply on a config written today), a
  policy that requires approval with no `approval_command` configured **denies**
  rather than passing silently, a change-set written to a newer contract version
  is never approvable, and `unclassified` accepts only `deny` or
  `require_approval` — auto-applying what nobody classified is not expressible.
  A refusal names which rule fired and which objects are responsible.

  `[policy]` validation reports a mistyped tier name (`"destructve"`) as a
  located error rather than letting it become an unlisted-and-therefore-denied
  tier, refuses a tier claimed by both lists instead of picking a winner, and
  warns when the approval hook and the tiers that need it disagree in either
  direction.

- **The blue-green window-safety rule now lives in `policy` as the always-on
  baseline.** `policy::evaluate` applies it *before* the tier policy and with or
  without a `[policy]` section: the tier policy is opt-in, the window-safety
  baseline is not, so folding the two gates into one decision function does not
  quietly delete today's blue-green block for anyone. `window_safe` stays on the
  wire as an *input* to the decision — it carries "confiture could not read this
  migration", which no risk tier expresses. The `window_safety` module it
  replaces is removed (see **Removed**).

- **The Confiture adapter reads the schema change-set, and advertises
  `risk_tier` only when the installed binary can produce one.** `preflight` now
  parses the `change_set` object out of `confiture migrate preflight --format
  json` into the typed `ChangeSet` the policy gate will read, and `describe`
  appends the **`risk_tier`** capability only when the detected confiture
  version is at or above the release that implements the contract. **No
  released confiture implements it yet** ([confiture#197][c197] is open, and
  0.40.0–0.42.0 shipped unrelated features), so the floor is `None` and the
  capability is currently never advertised — the consumer half ships complete
  and dormant, exactly as `window_safe` shipped ahead of confiture#154.
  Claiming the capability
  against a confiture that cannot classify would make the gate expect a
  change-set and deny every deploy: safe, and useless. Withholding it is the
  honest *"I do not classify"*, so a deploy with no risk policy configured
  behaves exactly as it does today, and a version string this adapter cannot
  parse degrades to *no capability*, never to *capability*.

  Every way of failing resolves to **unclassified**, which is never *safe*:
  a missing `change_set` key, an envelope that is not an object, a missing or
  non-integer `contract_version`, a `changes` key that is not a list, and an
  error envelope from a crashed confiture all yield no change-set at all. A
  `contract_version` from the future is *preserved* rather than swallowed, so
  `usable_change_set()` can name both versions in the refusal, and
  `ChangeSet::with_contract_version` (new, additive) is what lets an adapter
  reconstruct a wire payload without restamping it with this build's version.

  One asymmetry is deliberate: a broken **envelope** voids the whole
  change-set, while a broken **entry** becomes an unclassified placeholder that
  holds its position in the plan. Dropping the entry would shrink the set
  silently, and a shorter list of fully-classified changes reads as a *cleaner*
  plan than the truth. Warnings name the JSON *shape* that arrived and never
  quote its content: a payload that is off the contract has also left the
  contract's promise that `detail` carries no credential.

  The eight golden fixtures at
  `crates/fraisier-adapter-confiture/tests/fixtures/preflight/` are now a
  committed pact test — confiture asserts it emits those bytes, this adapter
  asserts it parses them — and a further test pins that this hand-rolled parser
  and `fraisier-core`'s `serde` path classify the same bytes identically.

- **The `command` migration adapter exposes the release context to its
  commands.** Every migration command now receives `FRAISIER_RELEASE_DIR` (the
  command's working directory — the staged release), and, when configured,
  `FRAISIER_ACTIVE_PATH` (the `active_path` symlink target) and
  `FRAISIER_APP_VERSION` (the version being deployed). A prepare script can
  reference the deploy's paths portably instead of coupling to a fixed
  `--app-version`. The two optional vars are omitted when their `[artifact]`
  settings are absent, so a script can tell "not configured" from an empty value.

- **`deploy --dry-run` shows the schema change-set and the policy verdict** —
  terraform-plan for schema change, in both human and JSON form. The plan lists
  every classified change (`tier`, `kind`, `object`, `migration`) **worst-first**
  — unclassified, then most severe down to least, stable within a tier so equal
  changes keep the order they will apply in — and states what the gate *would*
  decide, computed with the same `policy::evaluate` the live gate calls. A
  dry-run **never calls the approval hook**: it reports that sign-off would be
  needed rather than going to ask for it, which is structural — only
  `PolicyGate::admit` runs a hook. Blue-green's plan, previously four JSON
  fields and one line of prose, reaches the same parity and adds the
  window-safety verdict its baseline already reads.

  **Unknown is never rendered as zero.** *"The adapter looked and there is
  nothing to change"* is `change_set: { "changes": [], … }`; *"nobody looked, or
  nobody could classify"* is `change_set: null` with a
  `change_set_unavailable: { code, detail }` beside it — a stable machine code
  (`preflight_skipped`, `preflight_off`, `adapter_unavailable`,
  `no_preflight_capability`, `preflight_failed`, `no_risk_tier_capability`,
  `no_change_set`, `unreadable_change_set`), so a pipeline tells the two apart
  without parsing prose. The human render ends every such line with *"Risk is
  unknown, not zero."* Existing dry-run JSON keys keep their names and types;
  the additions are strictly additive.

  Every way of failing to see the schema **degrades rather than aborting**: an
  unreachable database, an unset DSN, an adapter with no `preflight`, a
  change-set from a contract this build cannot read. The plan still prints and
  still exits 0, because a plan *was* produced. Degradation reasons are stripped
  of URL credentials before they are printed or logged.

  **`--fail-on-block`** (new, on `deploy`) turns a would-be block into a nonzero
  exit for CI — both a refusal and a decision waiting on a human, because
  neither deploys unattended. Without a `[policy]` section there is no verdict,
  so it has nothing to gate on and never fires.

- **The policy gate runs, and an approval hook can unblock it.** `[policy]` now
  stops a deploy: the gate applies at the `preflight` step of all three
  strategies, ahead of the forward-compat lint, so a refused deploy stages
  nothing, migrates nothing and restarts nothing. A refusal names the objects
  responsible and opens with `policy refused:`.

  `approval_command` is the one way to unblock a change that needs sign-off —
  there is deliberately no `--force` or `--approve` flag, so "an agent with
  authority" means configuring a hook that can authenticate that agent. The hook
  runs via `sh -c` with the request as **JSON on stdin** (never argv) and the
  deploy's identity in `FRAISIER_APPROVAL_FRAISE` / `_ENVIRONMENT` /
  `_WORST_TIER` / `_CHANGE_COUNT`. Exit 0 approves, and the first non-empty line
  of stdout is recorded as the approver. **Every other outcome refuses** — a
  non-zero exit, a command that cannot be executed, a spawn failure, or a hook
  still running at `approval_timeout_secs` (now read; default 300s). The hook
  receives the identity and the triggering changes and no `AdapterCtx`, so no
  DSN or secret can reach it by construction.

  Which deploys are gated: single-host, multi-host and blue-green, opt-in by the
  section's presence (D6). `fraisier rollback` is **not** gated — it migrates
  `down`, while the preflight report describes the pending *forward* changes, so
  gating it would rule on a change-set the run will not apply and would put an
  approver between an operator and an emergency rollback.

  Every decision is audited: a `schema policy` tracing event with the deploy's
  identity (and the approver, on an approval), and a blocked deploy fires
  `[schedule].notify` with the event **`policy-blocked`** rather than
  `scheduled-deploy-failed`, so an unattended deploy held for sign-off is
  distinguishable from one that broke.

  `validate-config` now warns when `[policy]` is configured together with
  `[migration].preflight_mode = "off"`: nothing then inspects the pending schema
  changes, so every deploy is refused at the gate — correct, and baffling to
  debug from the refusal alone.

  Operator guide: [`docs/schema-risk-policy.md`](docs/schema-risk-policy.md).

- **One `describe` + `preflight` per deploy.** The lint, the blue-green window
  rule and the tier policy read one report instead of asking the adapter each —
  previously up to two confiture subprocesses and two database round-trips per
  deploy for answers that cannot differ within a run.

### Changed

- **BREAKING (adapter authors): `PreflightReport` is now `#[non_exhaustive]`,
  and carries a `change_set`.** The migration adapter contract can now carry a
  per-change **risk tier** across the seam as typed data, instead of inferring it
  from `issues[].code` strings. `PreflightReport` gains
  `change_set: Option<ChangeSet>`, and the new
  `RiskTier` / `SchemaChange` / `ChangeSet` / `RISK_CONTRACT_VERSION` vocabulary
  lands in `fraisier_core::adapter_axes`. Nothing reads the change-set yet — the
  policy gate that consumes it ships separately — so this release is a contract
  change with no behaviour change. `window_safe` is untouched and stays. The wire
  contract is specified in `docs/proposals/migration-risk-contract.md`, with
  golden fixtures at
  `crates/fraisier-adapter-confiture/tests/fixtures/preflight/`.

  The `#[non_exhaustive]` marker is the one deliberate break, taken now, pre-GA,
  so that every future field on this struct is additive. It forbids **all**
  struct-expression construction from outside `fraisier-core` — including
  `..Default::default()`, which is rejected just as a full literal is (E0639).
  An **in-process** adapter — one that links `fraisier-core` and returns this
  struct directly, as the bundled `confiture` and `command` adapters do — moves
  to the builder:

  ```rust
  // before
  PreflightReport { ok, issues, window_safe: Some(true) }

  // after
  PreflightReport::new(ok).with_issues(issues).with_window_safe(true)
  ```

  Reading a report is unchanged; only construction moves. `SchemaChange` and
  `ChangeSet` are `#[non_exhaustive]` for the same reason and ship with the same
  shape of builder (`SchemaChange::new(kind, object).with_tier(…)`,
  `ChangeSet::new(changes)`).

  **Out-of-process IPC adapters are unaffected**, whatever language they are
  written in: they serialise their own JSON per `PROTOCOL.md` and never name
  this type. The reference `fraisier-adapter-sqlx` is one of these — it declares
  no fraisier dependency at all, and the only fraisier code in its tree is a
  dev-dependency used by the integration test, which reads a report rather than
  building one. It needs no change.

  An adapter that classifies advertises the **`risk_tier`** capability, and only
  when the installed producer can actually emit a change-set. Consumers read the
  set through `PreflightReport::usable_change_set()`, which centralises the
  contract-version check: a change-set stamped with a version newer than this
  build understands is reported as unavailable — naming both versions — rather
  than best-effort parsed. Every way of being missing (no capability, no
  change-set, a version from the future, an entry with no recognised tier)
  resolves to *unclassified*, and unclassified is never *safe*.

- **BREAKING (CI pipelines): `deploy --dry-run` now reads the database.** A
  change-set can only come from `MigrationAdapter::preflight`, which for
  confiture means running `confiture migrate preflight` against the target
  database — so a dry-run needs a DSN where it previously needed nothing, spawned
  no process, and touched no database. **A CI job that runs `deploy --dry-run`
  as a cheap offline config check now needs either database credentials or
  `--skip-preflight`.**

  ```sh
  # before — offline, no credentials
  fraisier deploy --dry-run

  # after, unchanged output and still offline
  fraisier deploy --dry-run --skip-preflight
  ```

  `--dry-run --skip-preflight` restores the previous plan **byte-for-byte** (the
  machine form still records `change_set_unavailable.code =
  "preflight_skipped"`, so an agent cannot mistake the silence for *"no
  changes"*). Without the flag the dry-run degrades rather than failing: an
  unreachable database or an unset DSN yields the plan plus an explicit
  unavailability line, at exit 0. Blue-green's dry-run does **not** honour
  `--skip-preflight`, because its live preflight does not either — the
  window-safety rule is not something a flag can switch off.

### Removed

- **BREAKING (embedders): the `fraisier_core::window_safety` module is gone**,
  together with its `WindowSafety` type, `evaluate` and `check`. Its rule — the
  one that refuses a blue-green swap when confiture cannot certify the migration
  forward-compatible for a two-version window — is **not** gone: it is the
  always-on `Baseline::WindowSafety` half of `policy::evaluate`, applied with or
  without a `[policy]` section, and a blue-green deploy that is refused today is
  refused after this change for the same cause. Only the reason string is new
  (it now opens with `policy::REFUSED`).

  The break is taken now, pre-GA, so there is one gate rather than two places a
  future rule could be forgotten. An embedder calling the module directly moves
  to the one decision function:

  ```rust
  // before
  if let WindowSafety::Refused(reason) = window_safety::check(&*migration, &ctx).await {
      return Err(refuse(reason));
  }

  // after — one `describe` + `preflight` for the window rule *and* the policy
  let inspection = policy::inspect(&*migration, &ctx).await?;
  gate.admit(Baseline::WindowSafety, inspection.capabilities, inspection.report.as_ref(), &ctx)
      .await
      .map_err(refuse)?;
  ```

  `BlueGreenParams` gains a `policy: PolicyGate` field for the same reason;
  `PolicyGate::default()` is the "no `[policy]` section" value and keeps the
  window rule on.

## [1.0.0-beta.6] - 2026-07-23

### Changed

- **Migrations now run from the staged release directory.** On a single-host
  deploy, the migrate step runs the migration axis with its working directory set
  to the release just staged by `fetch` (`StagedArtifact.path`) instead of the
  operator's invocation directory. This unblocks the idiomatic **source-run /
  build-on-deploy** pattern: a `command`-adapter migration can now use the natural
  `up = "bash scripts/deploy/prepare.sh"` and have the script resolve against the
  release it was cut from — no more hard-coding an absolute release path coupled
  to `--app-version`. A confiture `--migrations-dir` given as a relative path
  resolves the same way, so the two adapters behave consistently. The override is
  gated on the staged path existing as a local directory, so a deploy with no
  artifact axis — or a remote single-host stage whose release lives on another
  host — keeps the previous base working directory. Preflight (which runs before
  `fetch`) is unaffected. Multi-host deploys are unchanged: they migrate once on
  the orchestrator, where the per-host release is not present.

## [1.0.0-beta.5] - 2026-07-21

### Added

- **`AdapterErrorKind` now carries confiture's full failure taxonomy.** Seven
  kinds were added so the confiture adapter maps each exit class 1:1 onto the wire
  instead of flattening most of them to `Execution`: `PreconditionFailed`
  (`-32004`, no migration ledger / `PRECON_1001`), `DbUnreachable` (`-32005`),
  `SchemaError` (`-32006`), `LockContention` (`-32007`, retriable), `GitError`
  (`-32008`), `IrreversibleRollback` (`-32009`), and `InternalError` (`-32010`).
  Each has a stable snake_case wire string matching confiture's semantic-class
  name. All are additive to the `#[non_exhaustive]` v1.0 enum (every downstream
  match already carries a wildcard arm), so the frozen contract is not broken.
- **`fraisier health` reports the live deployed version.** Alongside each host's
  health verdict, the command now surfaces the live deployed version and
  migration revision (`version` / `revision` in `--json`; a `version:` line in
  the pretty output), giving operators and CI a "what is actually live right
  now?" signal — parity with the Python `fraisier` `/health`. The version is read
  **only** from the committed release ledger (`DeployRecord`), which is written
  post-commit, so a deploy that fails before commit (e.g. at `migrate`) keeps
  reporting the *previous* version, never an in-flight one. The frozen
  `HealthStatus` / `HealthAdapter` probe API is unchanged. (#21)

### Changed

- **Confiture's exit-code classification now derives from a confiture-owned
  table.** The adapter previously encoded confiture's exit-code contract ad hoc
  inside `kind_for_code`, and the Python `fraisier` adapter encoded it *again,
  differently* — the two had drifted. Confiture is now the single source of truth:
  it emits the whole `(exit_code → semantic class)` table as JSON via
  `confiture --exit-codes-json`. This crate **vendors** that output
  (`src/exit_codes.vendored.json`) and the new `exit_codes` module projects it —
  `classify()` → `ExitClass` → `AdapterErrorKind`, a faithful 1:1 mapping. A
  contract test diffs the Rust table against the vendored file (always) and against
  the live `confiture --exit-codes-json` (when a new-enough confiture is on `PATH`;
  it skips otherwise), so a drift fails CI here and in confiture. The nine classes
  are `ok`, `internal_error`, `precondition_failed`, `db_unreachable`,
  `schema_error`, `invalid_config`, `lock_contention`, `git_error`,
  `irreversible_rollback`. A present exit code is authoritative and is never
  laundered by a stray error code — a config error (exit 5) is never downgraded to
  a benign precondition — and lock contention keeps its retriable message note on
  top of the distinct `LockContention` kind.
- **`check` / `ship` cap a failing check's console output to the tail.** When a
  failing check's combined stdout+stderr exceeds 30 lines, the report now prints
  only the last 30 (where pytest/ruff/mypy put their verdict) prefixed with a
  `... N earlier line(s) hidden` note, and writes the full output to a `0o600`
  log under `$XDG_DATA_HOME/fraisier/logs/ship-check-<name>-<stamp>.log`
  (falling back to `~/.local/share/...`), naming the path so nothing is lost.
  Short failures still print whole. Log writing is best-effort — an I/O error
  degrades to "tail only" rather than masking the failure. The `--json` report
  is unchanged (it still carries the full stdout/stderr). Parity with Python
  `fraisier` v0.36.0. (#20)

### Fixed

- **The Confiture `verify` gate no longer passes on error.** Confiture writes a
  structured *error envelope* to the same `--output` file a report would go to,
  on every error path — so the adapter's "we got JSON back, therefore we have a
  report" shortcut parsed envelopes as reports. An envelope carries no
  `failed_count`, which read as zero failures, so `verify` returned `ok = true`
  for *every* confiture failure, including an unreachable database. The deploy
  gate it feeds was unconditionally green on error. `verify` now recognises the
  envelope and surfaces Confiture's own diagnosis as an adapter error. The
  existing contract is unchanged: a report whose *checks* failed is still a
  valid result (`ok ⇔ failed_count == 0`), never an adapter error — only a
  non-report becomes one. This was pre-existing, not a Confiture 0.37.0
  regression: the same silent pass reproduces on 0.36.0.
- **`preflight` reports why it could not run.** It shares `verify`'s shape but
  failed *closed* (an envelope carries `"ok": false`), so it never passed on
  error — it reported a clean refusal instead: no issues, and no trace of the
  connection failure or missing ledger behind it. It now surfaces the envelope.
- **A database with no migration ledger is no longer called a config error.**
  Confiture 0.37.0 exits `2` with `PRECON_1001` for a database built from schema
  files rather than migrated. Exit `2` was mapped wholesale to `InvalidConfig`
  (JSON-RPC `-32602`), sending operators to fix a config file that was perfectly
  healthy. `PRECON_1001` now maps to its own `PreconditionFailed` kind (see
  *Added*) via the canonical exit-code table (see *Changed*): the operator is
  told to migrate, not to edit a healthy config. Exit `2` means "no ledger"
  under confiture's frozen contract, while a genuine configuration problem
  (exit `5`, e.g. `CONFIG_010`) still reports as `InvalidConfig`.

## [1.0.0-beta.4] - 2026-06-22

### Added

- **Restore-rehearsal migration preflight** (`[migration].preflight_mode =
  "live" | "restore_rehearsal" | "off"`, default `"live"`). In
  `restore_rehearsal` mode a deploy provisions a throwaway copy of the database
  from a backup (`preflight_backup_path`, else a fresh dump), rehearses the
  pending migrations there, and tears it down — *before* touching the live DB. A
  **full** restore carries the migration-tracking rows, so the pending set
  resolves from the throwaway's own tracking table; the rehearsal is
  self-consistent by construction, avoiding the Python-0.34 backup-behind-tracking
  bug rather than porting its fix. Opt-in (a full restore is heavy); the legacy
  `forward_compatible_lint = false` still maps to `"off"`. Escape hatch:
  `trigger-deploy --skip-preflight`. Confiture floor: ≥ 0.23 (window-safe verdict).
- **`ship --no-bump`** re-ships the current version (no bump, no version-file
  edit, no release commit — just (re)pushes `HEAD` to retrigger the deploy);
  mutually exclusive with a bump level. **Version-race detection**: before
  committing a bump, `ship` re-reads the version at `origin/<branch>` and, if it
  advanced, rolls back the on-disk bump and returns a named error with a
  copy-pasteable rebase recipe instead of a raw non-fast-forward git error.
- **Smoke-test token providers** for the http health probe (`[health].token_provider`):
  `exec`, `oauth2_client_credentials`, and `oauth2_refresh_token` acquire a
  short-lived bearer at deploy time and inject it (default `Bearer {token}` into
  `Authorization`), resolved at most once per deploy. Secrets resolve via
  `AdapterCtx::secret` (never config values); tokens and secrets never appear in
  logs or errors. `[health].headers` adds static probe headers. `validate-config`
  rejects a bad `format`, a header collision, and a token provider on a non-http
  adapter.
- **Webhook self-upgrade drain**: while a coordinated restart is draining
  (a `.draining` flag in the state dir), a verified deploy `POST` is refused with
  `503` + `Retry-After` + a JSON body naming the refused fraises, instead of being
  dropped. New defaulted `[webhook].self_upgrade_*` keys tune the drain
  (`drain_timeout_s` 600, `drain_poll_s` 1, `drain_settle_s` 2, `retry_after_s`
  60). See `docs/operations/self-upgrade.md`.
- **`scheduled install` drift policy + `--prune`**: each unit is classified
  Absent / Identical / Drifted; Identical is an idempotent no-op, Drifted fails
  the install closed unless `--force`, and `--prune` removes marker-bearing
  scheduled units no longer declared (reusing the existing prune marker
  machinery).
- **`fraisier doctor`** (host self-diagnosis: config loads/validates, referenced
  secrets readable, confiture ≥ floor; exits 0/1/2 = pass/fail/warn) and
  **`fraisier env-check <subcommand>`** (which env-var secrets a subcommand reads,
  and which are unset; exits 0/1/2).
- **Global `--verbose`/`-v`** (repeatable, mutually exclusive with `--json`).
  Output stays compact by default and never auto-upgrades under
  `CI`/`CLAUDECODE`/no-TTY.

### Changed

- **`validate-config` is structure-only by default** (no secrets resolved);
  `--resolve-envvars` adds a pre-deploy CI gate that fails when any referenced
  `*_env` source variable is unset.

## [1.0.0-beta.3] - 2026-06-11

### Added

- **Command health adapter** (`[health].adapter = "command"`): a `HealthAdapter`
  that runs a configured shell command as the saga's post-deploy `health` step —
  the gate passes iff the command exits `0`. Spawn failure and a (configurable,
  default 60s) timeout fail closed: every ambiguous outcome rolls the deploy back
  rather than committing a release as healthy. The DSN reaches the command by
  environment (inherited from `[migration].database_url_env`), never argv.
- **Perf-regression rollback gate**: with `fraiseql perf regression-scan
  --fail-on-regression --json` (FraiseQL v2.6.0+), the command health adapter
  parses the scan's report and names the top regressed operation — e.g.
  `perf regression: order/UPDATE p50 +42% (12ms→17ms), 3 more` — in the rollback
  reason and the `[schedule].notify` failure webhook. Without `--json` the gate
  still works; the detail degrades to a plain output excerpt. A migration-agnostic
  alternative to the command-migration `verify` hook. See
  `docs/perf-regression-gate.md`.

## [1.0.0-beta.2] - 2026-06-08

### Added

- **Project checks** (`fraisier check` + `fraisier-check` crate): a declarative
  `[[checks]]` list in `fraisier.toml` — named shell commands run via `sh -c`
  with cross-check parallelism (`-j`, default auto). One source of truth runnable
  locally and in CI. A check passes iff its command exits `0`; output is captured
  and shown for failures (and always under `--json`).
- **`ship` check gate**: `fraisier ship` runs `[[checks]]` before the version
  bump and refuses to release if any fails. On by default; `--no-check` skips it;
  a project with no `[[checks]]` ships exactly as before; `--dry-run` reports the
  checks would run without executing them.

## [1.0.0-beta.1]

The first public release: a deploy-orchestration engine with atomic,
migration-safe rollback across one host or a fleet.

### Engine

- **Saga engine** (`fraisier-saga`): ordered compensable steps with reverse-order
  rollback and a durable release ledger; outcomes are committed, rolled back, or
  partially rolled back. The `Saga` / `Step` / `StepContext` / `SagaError` /
  `SagaOutcome` API is the frozen v1.0 contract.
- **Pluggable state store**: filesystem, sqlite, and in-memory backends behind
  one `StateStore` trait, with key enumeration.
- **Five adapter axes** — `artifact`, `migration`, `service`, `health`, `lb` —
  each usable in-process (Rust trait) or out-of-process over a JSON-RPC-over-stdio
  protocol, so adapters can be written in any language. Adapters receive secret
  names, never values.

### Deploy flows

- **Single-host deploy**: `preflight → fetch → migrate → activate → restart →
  health → verify`, with atomic rollback (prior release re-activated from the
  ledger, migration rolled down) on any failure.
- **Multi-host deploy**: rolling and all-at-once rollouts across an SSH fleet
  (shell-out or IPC transport with OpenSSH `ControlMaster` reuse); migrate once on
  the orchestrator, per-host rollout with load-balancer drain/reattach, reverse-
  order fleet rollback.
- **Blue-green deploy**: a forward-compatibility window-safety hard gate
  (consuming confiture's first-class `window_safe` verdict, confiture ≥ 0.23.0), a
  pre-swap connection-budget probe, an nginx traffic swap, and swap-back to the
  still-hot blue fleet if green degrades during the hold (no database rollback).

### Adapters

- artifact: release tarball, git, local path, host-pull, IPC.
- migration: confiture (DSN via environment + `--no-config`), command, IPC.
- service: systemd, rc, docker-compose.
- health: http.
- lb: nginx (upstream-include symlink swap).

### CLI & operations

- `init`, `validate-config`, `deploy` (`--dry-run`, `--host`, `--app-version`),
  `list`, `status` (`--per-host`), `health`, `rollback`.
- `bootstrap` (prepare host directories over SSH/locally).
- `webhook-server`: HMAC-signed, replay-protected deploy trigger, socket-activated
  or standalone.
- `scheduled` (systemd timer/service install + CRUD) with an unattended-deploy
  safety gate and failure notification.
- `self-upgrade` (coordinated graceful restart).
- `sync` (share the deploy ledger across operators over git refs; experimental).
- `backup` and `db` (migrate / restore / reset) for generic-Postgres lifecycle.
- `providers` / `provider-test` (enumerate and probe adapters per axis).
- `version` / `ship` (version bump, commit, push, deploy).
- `scaffold` / `scaffold-install` (generate and install systemd/socket/nginx/CI
  files).

### Observability & supply chain

- Every saga step is exported as an OpenTelemetry span and persisted to the state
  store.
- Workspace-wide `unsafe_code = "forbid"`; `cargo deny` gate over advisories,
  licenses, bans, and sources.

### Developer tooling

- **Task runner** (`crates/xtask`, zero dependencies): `cargo xtask ci` runs the
  full gate (fmt, clippy `-D warnings`, test, release build, `cargo deny`,
  shellcheck) — the same command CI invokes, so local and CI cannot drift.
  `cargo xtask dist` cross-builds the static musl binary via `cargo-zigbuild`.
- **GitHub Actions CI** that runs `cargo xtask ci`.

[c197]: https://github.com/fraiseql/confiture/issues/197
[Unreleased]: https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.11...HEAD
[1.0.0-beta.11]: https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.10...fraisier-v1.0.0-beta.11
[1.0.0-beta.10]: https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.9...fraisier-v1.0.0-beta.10
[1.0.0-beta.9]: https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.8...fraisier-v1.0.0-beta.9
[1.0.0-beta.8]: https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.7...fraisier-v1.0.0-beta.8
[1.0.0-beta.7]: https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.6...fraisier-v1.0.0-beta.7
[1.0.0-beta.6]: https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.5...fraisier-v1.0.0-beta.6
[1.0.0-beta.1]: https://github.com/fraiseql/fraisier-core/releases/tag/v1.0.0-beta.1

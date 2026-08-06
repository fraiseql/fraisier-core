# Changelog

All notable changes to fraisier are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
[Unreleased]: https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.7...HEAD
[1.0.0-beta.7]: https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.6...fraisier-v1.0.0-beta.7
[1.0.0-beta.6]: https://github.com/fraiseql/fraisier-core/compare/fraisier-v1.0.0-beta.5...fraisier-v1.0.0-beta.6
[1.0.0-beta.1]: https://github.com/fraiseql/fraisier-core/releases/tag/v1.0.0-beta.1

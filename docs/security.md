# Security model & deliberate non-ports

This page records two things: the rules the engine actually enforces, and the
Python-`fraisier` surface that is deliberately left unported because a different
(and usually simpler) design is already in place — so those read as intent, not
omission.

## Credentials never ride a failure reason

An adapter builds its error out of whatever its tool wrote to stderr, and a
database client that cannot connect prints the DSN it tried, password included.
That text becomes the saga's failure reason, and a reason is the most widely-copied
string the engine produces: printed, logged to the tracing/OTel pipeline, persisted
to the state store, replicated to a remote ledger, and sent to the
`[schedule].notify` webhook.

So the reason is redacted — at the **boundaries**, never at the places a reason is
built. There are around twenty-five of those and one more appears with every new
step; there are four boundaries, and they cover the steps nobody has written yet:

| Boundary | Covers |
|---|---|
| The CLI's output edge (`rendered`) | every command's text, its `--json` payload, its `--verbose` stderr JSON |
| A rendered anyhow chain (`error_detail`) | the CLI's `error:` line, and the webhook's HTTP 500 body — which leaves the host without passing the output edge |
| The notify sink (`emit_event`, `ExecHookNotifier`) | the hook's env var and stdin JSON, and the notifier's log event — which fires with no hook configured. At the **sink**, so it holds for every `FailurePayload` producer, this crate's and an embedder's alike |
| `Saga::rollback` | the `PartialRollback` reason before it is persisted, and so `state.json`, `events.jsonl` and `sync push` |
| The IPC client's `data` fold | a remote adapter's stderr, which the fold puts into the message every sink renders |
| `fraisier-self-upgrade`'s `download` | an artifact URL's basic-auth credentials in a fetch error, which reach a library consumer through `AbortedBeforeSwap` without passing the CLI edge |

Two forms are stripped: a URL's `user:password@`, and libpq's `password=` keyword
(bare or single-quoted). Everything an operator needs in order to act is kept —
which host could not be reached, which step failed, what the tool said — because a
reason that has been scrubbed into uselessness gets worked around.

`fraisier_saga::redact::credentials` is the one implementation, re-exported as
`fraisier_core::redact`. It lives in the engine crate because the engine is what
persists and replicates a reason, and because `fraisier-core` depends on
`fraisier-saga` and not the other way round.

Two of those boundaries sit inside `fraisier-self-upgrade` rather than in the
binary, because that crate is published and embeddable: a consumer that calls
`apply()` or drives a `Notifier` itself never passes the CLI's output edge, so a
guarantee placed only there would not reach them.

**What this does not cover.** Only credentials, and only those two forms. Any other
secret an adapter writes to stderr — an API token, a signing key — still travels,
so keep secrets out of the stderr of anything fraisier runs. `AdapterError.stderr`
also keeps its raw text on purpose: it is a structured field nothing renders today,
and the output edge covers it if something ever does.

## Privileged operations: `systemctl` shell-out, not socket helpers

Python fraisier shipped root-privileged Unix-socket helpers with `SO_PEERCRED`
peer-credential checks (v0.29) to let an unprivileged deploy user install units
and toggle services. fraisier-core does **not** port the socket helpers or
`SO_PEERCRED` (gap-matrix Row 13 — **WON'T** for v1.0).

Instead, per PRD §3.3, privileged actions shell out to `systemctl` directly and
rely on the **unit-file permissions + deploy-user model**: `/etc/systemd/system`
is root-write-only, scaffolded units and sudoers fragments scope what the deploy
user may do, and `scaffold-install` / `scheduled install` run under operator-typed
`sudo`. The marker convention (`fraisier-generated: …`) used by `--prune` is
**advisory, not authenticated** — it scopes honest cross-project/cross-env
mistakes, not adversaries; `/etc/systemd/system` being root-only-write is the real
trust boundary. D-Bus / privileged socket helpers (and a `--via-socket` path that
drops the operator-sudo requirement) are deferred to v1.1+.

## `fraises.yaml` runtime compatibility — withdrawn

The Python `fraises.yaml` format (with the `!envvar` YAML tag) is **not** parsed
at runtime (gap-matrix Row 14 — **WON'T**; Decision 2026-06-02). `fraisier.toml`
is the single source of truth; secrets are referenced per field by their *source
env var name* (`database_url_env`, the webhook `secret_env`, the OAuth2
`client_secret_env`/`refresh_token_env`, …) and resolved lazily via
`AdapterCtx::secret` (Decision 5) — values never enter config or argv. Convert a
`fraises.yaml` by hand; `fraisier init` writes a starter `fraisier.toml` and
`validate-config` (optionally `--resolve-envvars`) checks it.

## Branch promotion & GitHub-PR releases — left to CI / `gh` / release-plz

Python `ship` drives GitHub PRs + auto-merge, and Python `sync` promotes git
source branches dev→staging via PR. fraisier-core keeps these **out of the tool**
(gap-matrix Rows 5 & 12 — **WON'T**; Decision 2026-06-22):

- `ship` does a direct **bump → commit → push** (+ optional deploy) and runs
  `[[checks]]` as a pre-bump gate; version-race detection guards concurrent ships.
  GitHub PR creation + `--auto`-merge releases live in CI / `gh` / **release-plz**,
  which is this project's release model.
- `sync` is the **deploy-state ledger** shared over `refs/fraisier/sync/<fraise>/<env>`
  — a different feature from git branch promotion. Promote source branches with
  CI / `gh` (e.g. a `gh pr create … && gh pr merge --auto` workflow); fraisier-core
  does not own that flow.

# Security model & deliberate non-ports

fraisier-core deliberately leaves some Python-`fraisier` surface unported, because
a different (and usually simpler) design is already in place. This page records
those decisions so they read as intent, not omission.

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

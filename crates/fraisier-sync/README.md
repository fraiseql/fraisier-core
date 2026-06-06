# fraisier-sync (experimental)

Share the deploy **ledger** across operators over git refs — no bespoke server.
Each fraise/env's state lives as a commit chain under `refs/fraisier/sync/<key>`
on a git remote the team already has.

## Why git refs

Git gives optimistic concurrency for free: a non-fast-forward `push` is
**rejected**, which is exactly the conflict check a shared mutable ledger needs.
fraisier never force-pushes — a rejected push surfaces as a conflict, and the
operator reconciles by pulling (accepting the remote) before re-pushing. The
commit chain is the history.

A persistent local bare repo (the `sync_dir`) holds each ref at the last-synced
commit; that local ref is the **sync base**. A push parents on it, so if the
remote moved since, git rejects the push rather than silently overwriting another
operator's state.

## Experimental

The on-ref format (a `state.json` blob per commit) is **not** a stability
commitment in v1.0 — it may change before GA. The CLI warns on use.

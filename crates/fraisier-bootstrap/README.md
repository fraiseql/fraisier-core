# fraisier-bootstrap

SSH-based host bootstrap for [fraisier](../../README.md): prepare a host's
filesystem so a deploy can stage and activate artifacts on it.

It reuses the `Local | Ssh` transport the deploy uses, so the same mechanism
prepares a single-host config locally and a multi-host config on each host over
`ssh`. For beta the scope is deliberately narrow: create the directories a
deploy needs — `[artifact].staging_dir` and the directory holding `active_path`.
`mkdir -p` is idempotent, so re-bootstrapping a host is safe.

Package and unit installation are out of scope here: use `scaffold-install` for
the systemd/nginx files, and the operator (or a subprocess fallback) for any
package prerequisites — the PRD allows that for beta.

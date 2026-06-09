# fraisier-self-upgrade

The engine behind `fraisier self-upgrade apply`: fetch a new fraisier binary,
verify its SHA-256, stage it beside a stable `current` symlink, atomically swap
the symlink, restart the supervised unit, health-check it, and **auto-revert to
the kept-old binary** if the new one fails to come up healthy.

The controller is a short-lived process distinct from the supervised webhook
unit, and it drives the swap through **systemd + an out-of-process HTTP health
probe only** — it never `exec`s the binary it just swapped, so a revert survives
even a binary that boots-then-dies.

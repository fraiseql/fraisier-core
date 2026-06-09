# fraisier-check

The declarative check runner behind `fraisier check` and the gate `fraisier
ship` runs before a release.

A [`Check`] is a named shell command. [`run`] executes a slice of them with a
bounded number running concurrently (`jobs`) and returns a [`CheckRunReport`]
whose outcomes stay in the original (config) order regardless of which finished
first. A check passes iff its command exits `0`; a non-zero exit is a failure and
a command that cannot be spawned is a spawn error — both fail the run.

Cross-check parallelism is the runner's `jobs`. Intra-check parallelism (for
example `pytest -n auto`) lives inside the command string, so the runner needs no
per-framework knowledge.

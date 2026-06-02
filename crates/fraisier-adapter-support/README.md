# fraisier-adapter-support

Shared building blocks for fraisier's in-process adapters. Intentionally
policy-free — it provides mechanism, each adapter keeps its own interpretation.

- **`run_command`** — spawn a subprocess, capture stdout/stderr/exit code, and
  turn a *spawn* failure into a tagged `AdapterError`. A process that runs and
  exits non-zero is returned as `Ok(Captured)`; the caller maps the exit code to
  its own `AdapterErrorKind`. Used by the shell-out adapters (`command`,
  `systemd`).
- **`retry_on_err`** — retry an async fallible operation up to N times with a
  fixed delay, returning the first `Ok` or the last `Err`. Used by the network
  adapters (`http` health, `release` artifact download).

Depends on `fraisier-core` only (for `AdapterError`); it never depends on
`fraisier-ipc`.

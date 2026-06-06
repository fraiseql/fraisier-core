# fraisier-core

Deploy-specific composition over the [`fraisier-saga`](../fraisier-saga) engine.

This crate holds the deploy layer of [fraisier](https://github.com/fraiseql/fraisier)
(PRD §5.1, Layer 2): the five **adapter axis traits** (artifact, migration,
service, health, load-balancer) and the multi-host plan. The saga engine stays
generic; the deploy semantics live here.

## Architecture rule

`fraisier-core` defines the adapter traits and depends on `fraisier-saga` only.
It must **never** depend on `fraisier-ipc` — concrete adapter selection
(in-process vs IPC) is wired at the `fraisier` binary / embedder layer. This keeps
the trait definitions strictly upstream of every implementation.

## License

Licensed under either of MIT or Apache-2.0 at your option.

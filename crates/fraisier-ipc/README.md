# fraisier-ipc

The JSON-RPC-over-stdio IPC client for fraisier **migration adapters**.

External adapters are binaries on `PATH` named `fraisier-adapter-<name>`. This
crate's [`IpcMigrationAdapter`] spawns such a binary, speaks the adapter protocol
to it over stdin/stdout (LSP-style `Content-Length`-framed JSON-RPC), and
implements the `fraisier_core::adapter_axes::MigrationAdapter` trait by doing so —
so an IPC adapter is a drop-in for an in-process one (the convergence rule).

The wire protocol every adapter author implements against is specified in
[`PROTOCOL.md`](./PROTOCOL.md), independent of this Rust implementation.

## Crate-graph

`fraisier-ipc` depends on `fraisier-core` (to implement its trait). `fraisier-core`
never depends on `fraisier-ipc`; concrete adapter wiring lives at the `fraisier`
binary / embedder layer.

## License

Licensed under either of MIT or Apache-2.0 at your option.

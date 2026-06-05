# fraisier-adapter-release

A first-party [fraisier](https://fraiseql.dev) **artifact** IPC adapter. It speaks
the JSON-RPC-over-stdio adapter protocol (v1 — see
[`../fraisier-ipc/PROTOCOL.md`](../fraisier-ipc/PROTOCOL.md)) and backs each call
with the in-process `release` adapter: download a release archive over HTTP, verify
its SHA-256, stage it, and activate it via an atomic symlink swap.

## Why a binary

It is the **host-side half of fraisier's IPC-over-SSH artifact path**. The
orchestrator launches it on each host —

```
ssh <host> -- fraisier-adapter-release
```

— and the framed JSON-RPC request/response flow through ssh's stdio. The adapter
does its filesystem/HTTP work *locally on the host*, so a host needs only this one
binary (no `curl`/coreutils), and the orchestrator never holds the release bytes.
OpenSSH `ControlMaster` multiplexing (set up by the client) amortises the
connection across a deploy's per-host calls.

It is **one-shot per call**: read one framed request from stdin, dispatch, write one
framed response to stdout, exit — so a crash never outlives a single call. The same
binary also serves a single-host local deploy (`[artifact] source = "release-ipc"`).

## Methods

`describe`, `stage`, `activate`, `current` — the `ArtifactAdapter` axis. The release
configuration (`version`, `release_url`, `sha256`/`checksum_url`, `staging_dir`,
`active_path`) travels in `params.ctx.settings`, exactly as for the in-process
adapter. No secret crosses the wire.

## Install (host bootstrap)

```
cargo install --path crates/fraisier-adapter-release   # or copy the built binary
```

A real `fraisier bootstrap` (Phase 3) will place it on each host; the multi-host
fixture `scp`s it.

# Fraisier adapter IPC protocol

**Protocol version:** 1

This is the contract a `fraisier-adapter-<name>` binary implements, in any
language. Fraisier spawns it as a child process and exchanges one JSON-RPC 2.0
request/response per call over the child's stdin/stdout.

The protocol is **axis-generic**: each method name is a trait method of one of the
adapter axes (migration, artifact, …), `params` carry its (serializable) arguments,
and `result` is its (serializable) return value (the convergence rule). The
**migration** axis is selected by `migration.adapter = "<name>"` and the binary is
discovered on `PATH`. The **artifact** axis is selected by
`artifact.source = "release-ipc"` (or a configured `adapter_bin`); a first-party
example is [`fraisier-adapter-release`](../fraisier-adapter-release).

The same binary runs either **on the orchestrator** (launched locally) or **on a
target host** (launched as `ssh <host> -- <binary>`, the framed JSON-RPC flowing
through ssh's stdio) — the adapter logic is identical; only the launch location
changes.

## Framing

Each message is framed LSP-style:

```
Content-Length: <byte-length-of-body>\r\n
\r\n
<body>
```

`<body>` is a single UTF-8 JSON value of exactly `<byte-length-of-body>` bytes.
Headers other than `Content-Length` are ignored. The stream may carry multiple
messages back-to-back; in v1 the core sends one request, closes stdin, and reads
one response.

## Request

```json
{ "jsonrpc": "2.0", "id": 1, "method": "<method>", "params": { "ctx": { … }, … } }
```

## Response

Success:

```json
{ "jsonrpc": "2.0", "id": 1, "result": <result> }
```

Error (the `code` is preserved into the host's `AdapterError.code`; `data`, if
present, is appended to the message):

```json
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32010, "message": "…", "data": { … } } }
```

The `id` echoes the request `id`. Reserved code ranges follow JSON-RPC: `-32601`
method not found, `-32602` invalid params, `-32600` invalid request, `-32700`
parse error. Adapters use their own codes (e.g. `-32000…-32099`) for execution
failures.

## The `ctx` object

Every method except `describe` takes `params.ctx`, the serialized `AdapterCtx`:

| Field | Type | Notes |
|---|---|---|
| `fraise` | string | deployable name |
| `environment` | string | target environment |
| `host` | string \| null | per-host operations only |
| `workdir` | string (path) | working directory to assume |
| `migrations_path` | string (path) \| null | migrations directory |
| `env_secrets` | object (string→string) | logical name → logical name (identity-mapped by the core) |
| `previous_revision` | string \| null | for rollback diagnostics |
| `artifact_ref` | object \| null | currently staged/active artifact |
| `settings` | object | adapter-specific config from `fraisier.toml` |

### Secrets

Secret **values never appear** in the JSON. For each entry in `env_secrets`, the
core sets a matching **environment variable** on the adapter process whose value
is the real secret. The adapter reads, e.g., `DATABASE_URL` from its environment
(`std::env`/`os.environ`), never from `params`.

## Methods

| Method | `params` (besides `ctx`) | `result` | Optional? |
|---|---|---|---|
| `describe` | *(none — no `ctx`)* | `{ name, version, protocol_version, capabilities: [string] }` | required |
| `current_revision` | — | `string \| null` (revision) | required |
| `up` | `target: string \| null` | `MigrationOutcome` | required |
| `down_to` | `target: string` | `MigrationOutcome` | required |
| `verify` | — | `{ ok: bool, checks: [{ name, ok, detail }] }` | required |
| `preflight` | — | `{ ok: bool, issues: [{ severity, code, message, migration }] }` | optional |
| `post_migrate` | — | `null` | optional |

`MigrationOutcome` = `{ from: string|null, to: string|null, applied: [string], log: string }`.

### Artifact axis methods

When the binary backs the **artifact** axis, it implements the `ArtifactAdapter`
methods instead. Each per-host method carries `params.ctx` (the artifact config is
in `ctx.settings`: `version`, `release_url`, `sha256`/`checksum_url`, `staging_dir`,
`active_path`) and `params.host`:

| Method | `params` (besides `ctx`) | `result` |
|---|---|---|
| `describe` | *(none)* | `{ name, version, protocol_version, capabilities: ["stage","activate","current"] }` |
| `stage` | `host: string` | `StagedArtifact` |
| `activate` | `host: string`, `staged: StagedArtifact` | `null` |
| `current` | `host: string` | `ArtifactRef \| null` |

`ArtifactRef` = `{ id: string, checksum: string|null }`;
`StagedArtifact` = `{ artifact: ArtifactRef, path: string }`. No secret crosses the
wire (the artifact axis needs none); a remote (ssh-launched) adapter carries no env.

### Optional methods and capabilities

`describe.capabilities` lists the methods the adapter actually implements (by
name). The host **only calls `preflight`/`post_migrate` if advertised**:

- `preflight` absent → the host skips the forward-compat lint and logs a warning
  (it never treats "not implemented" as "lint passed"). An adapter that does not
  implement it should return JSON-RPC error `-32601`.
- `post_migrate` absent → the host treats it as a no-op.

`describe.protocol_version` must be `1`; the host rejects adapters whose major
version it does not speak.

## Discovery

Binaries named `fraisier-adapter-<name>` on `PATH`. `fraisier adapter list`
enumerates them; `fraisier adapter describe <name>` calls `describe`.

# Fraisier migration adapter IPC protocol

**Protocol version:** 1

This is the contract a `fraisier-adapter-<name>` binary implements, in any
language. Fraisier discovers the binary on `PATH` (selected by
`migration.adapter = "<name>"`), spawns it as a child process, and exchanges one
JSON-RPC 2.0 request/response per call over the child's stdin/stdout.

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

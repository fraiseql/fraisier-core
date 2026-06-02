# fraisier-adapter-nginx

The reference **load-balancer** adapter for fraisier — the `LbAdapter` axis (PRD
§6.1). It drains a host from an nginx `upstream` and reattaches it by toggling the
`down` flag on the host's `server` directive and reloading nginx.

The trait is public so others can add HAProxy / Caddy / cloud-LB adapters; this is
the reference implementation Phase 4's multi-host rollout drives.

## Configuration (`[lb]` table → `AdapterCtx.settings`)

```toml
[lb]
adapter = "nginx"
config_path = "/etc/nginx/sites-available/fraiseql"   # file holding the upstream
upstream = "fraiseql_upstream"                         # the `upstream <name> { … }`
```

The host is matched by address — the multi-host deploy supplies it as
`settings["address"]`, falling back to the inventory `HostId`. A `server` line
like:

```nginx
upstream fraiseql_upstream {
    server web1.internal:8080 weight=5;
    server web2.internal:8080;
}
```

- **drain** → `server web1.internal:8080 weight=5 down;`, then `nginx -s reload`;
  returns the host's prior `LbMembership` (state + weight).
- **reattach** → clears `down` when the captured prior membership was `InPool`,
  then reloads.

The edit is **atomic** (a `<config>.bak` backup, then a temp file renamed over the
target), so a reload never observes a half-written config. Only the `down` flag is
toggled; an existing `weight=` is preserved (and reported) but not rewritten.

The `nginx` binary is resolved on `PATH`, overridable via `FRAISIER_NGINX_BIN` or
`NginxLb::with_program`.

## Tests

Unit tests cover the upstream/`server` parsing and the `down`-flag rewrite; an
integration test drives a full drain → reattach round-trip against a real config
file and a fake `nginx` (so no real nginx is needed).

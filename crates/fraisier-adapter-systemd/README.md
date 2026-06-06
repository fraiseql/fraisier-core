# fraisier-adapter-systemd

The systemd `ServiceAdapter` for fraisier. It restarts and reports the status of
a systemd unit by shelling out to `systemctl` (PRD §3.3 — D-Bus is a v1.1+
upgrade).

## Configuration

Read from the `[service]` settings table:

```toml
[service]
adapter = "systemd"
unit = "fraiseql.service"
user = false              # optional: systemctl --user
```

- `restart` → `systemctl [--user] restart <unit>` (non-zero exit = failure).
- `status` → `systemctl [--user] is-active <unit>`; `running` is true only when
  the unit reports `active`. `is-active` exits non-zero for an inactive unit, so
  the exit code is informational — only a spawn failure is an error.

Override the binary with `FRAISIER_SYSTEMCTL_BIN` (used in tests). The adapter
runs `systemctl` locally; the `host` argument is reserved for the SSH dispatch
layer. The adapter assumes no privilege — escalation is the operator's concern.

# fraisier-adapter-rc

The FreeBSD rc.d `ServiceAdapter` for fraisier. It restarts and reports the
status of an rc.d service by shelling out to `service(8)` (PRD §6.3).

## Configuration

Read from the `[service]` settings table:

```toml
[service]
adapter = "rc"
name = "fraiseql"     # the rc.d service name (the `service <name> …` argument)
```

- `restart` → `service <name> restart` (non-zero exit = failure).
- `status` → `service <name> status`; `running` is read from the status text
  (`"is not running"` is checked before `"is running"`), falling back to the exit
  code when the script prints neither phrase. A stopped service exits non-zero,
  so the exit code is informational — only a spawn failure is an error.

Note the argument order: `service` takes the name *before* the command
(`service nginx restart`), unlike `systemctl restart nginx`.

Override the binary with `FRAISIER_SERVICE_BIN` (used in tests). Phase 1/2 run
`service` locally; the `host` argument is reserved for the Phase 3+ SSH dispatch
layer. The adapter assumes no privilege — escalation is the operator's concern.

## Integration testing

Real `service(8)` exercises need a FreeBSD host or jail and are deferred to
Phase 5; the parsing and argv construction are unit-tested here on any platform.

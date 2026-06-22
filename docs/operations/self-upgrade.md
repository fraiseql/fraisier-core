# Webhook self-upgrade drain

On multi-environment hosts a single webhook server can receive a second deploy
while it is restarting itself for a self-upgrade. Without coordination that deploy
is dropped (the caller sees a connection error). fraisier drains instead.

## Behaviour

A coordinated restart raises a **drain flag** — the file `.draining` in the
webhook's state directory — *before* restarting. While that flag exists:

- a signature-verified deploy `POST` is answered with **`503 Service
  Unavailable`**, a **`Retry-After`** header, and a JSON body naming the refused
  fraises:

  ```json
  { "status": "draining", "retry_after_s": 60, "refused": ["checkout"] }
  ```

  so upstream callers (GitHub Actions, `curl`, monitors) record a loud, retriable
  failure rather than a silent drop;
- `GET /healthz` is unaffected (liveness stays up for the load balancer).

The restart coordinator ([`fraisier_webhook::drain_in_flight`]) raises the flag,
waits a **settle** window so dispatch-accepted deploys reach their state-store
lock, then polls until every in-flight deploy lock has cleared (bounded by a
**timeout**) and only then issues the restart. On a drain **timeout** it lowers
the flag (the server resumes serving), leaves the unit **unrestarted** for
operator intervention, and the caller surfaces a distinct exit code while logging
the still-held lock names.

## Tuning (`[webhook]`)

All four keys are optional and defaulted; existing configs pick up the behaviour
with no change:

| Key | Default | Meaning |
|---|---|---|
| `self_upgrade_drain_timeout_s` | `600` | Max wait for in-flight deploys before giving up. |
| `self_upgrade_drain_poll_s` | `1` | How often to re-check for in-flight deploys. |
| `self_upgrade_drain_settle_s` | `2` | Settle window after raising the flag. |
| `self_upgrade_retry_after_s` | `60` | `Retry-After` value on the 503 refusal. |

## Lock-backend caveat

The in-flight signal is the filesystem state-store lock (`<state>/<fraise>.lock`).
The coordination is exact for the default file backend. A future SQL/`database`
lock backend sees no lock files and would drain immediately — no worse than not
draining at all; a SQL-aware drain probe is a follow-up.

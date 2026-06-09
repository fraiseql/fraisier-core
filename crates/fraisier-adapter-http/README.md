# fraisier-adapter-http

The HTTP `HealthAdapter` for fraisier. It probes a host with a `GET` and retries
until the host is healthy or the attempts are exhausted (retry via
`fraisier-adapter-support`).

## Configuration

Read from the `[health]` settings table:

```toml
[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"   # {host} is replaced with the host id
expected_status = 200                   # default 200
attempts = 3                            # default 3
retry_delay_ms = 500                    # default 500
timeout_ms = 5000                       # default 5000
```

## Healthy vs. unreachable

The adapter honours the trait's distinction between a probe *result* and a probe
*failure*:

- response status == `expected_status` → `Ok(healthy: true)`
- response with any other status → `Ok(healthy: false)` (a result, not an error)
- transport failure (refused, timeout, DNS) after every attempt → `Err(..)`

TLS uses rustls (no OpenSSL).

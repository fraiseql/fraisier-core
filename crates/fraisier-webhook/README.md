# fraisier-webhook

The webhook server for [fraisier](../../README.md): HMAC-signed,
replay-protected HTTP POSTs that trigger a deploy, served over systemd **socket
activation** or a **standalone** listener.

## Signed-request scheme

A request carries two headers:

- `X-Fraisier-Timestamp` — the Unix-seconds timestamp it was signed with;
- `X-Fraisier-Signature` — `sha256=<hex>` of `HMAC_SHA256(secret, "<timestamp>.<body>")`.

The server verifies the signature **in constant time** and rejects any request
whose timestamp is outside the configured replay window. Folding the timestamp
into the signature is what makes a captured request unusable once it is stale —
an attacker cannot move the timestamp without breaking the signature.

The shared secret is supplied via an environment variable (never config or
argv), consistent with fraisier's secret-handling rule.

This crate provides the transport and verification only; the deploy a verified
request triggers is wired by the CLI, so the crate stays independent of the
deploy composition. Run it behind a reverse proxy or on a trusted network.

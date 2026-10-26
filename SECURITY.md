# Security policy

## Reporting a vulnerability

Report a vulnerability privately through GitHub's private vulnerability reporting:
open the repository's **Security** tab and choose **Report a vulnerability**. The
report is visible only to the maintainers until an advisory is published.

Please do not open a public issue, pull request or discussion for a suspected
vulnerability.

A useful report says which version you ran (`fraisier --version`), which
configuration sections were involved, and how to reproduce the problem. Leave
real credentials, DSNs and customer data out of it.

## Supported versions

fraisier is in beta. Until 1.0.0, only the **latest published beta** receives
security fixes. A fix ships as a new beta, and the advisory names the first
fixed version.

| Version | Supported |
|---|---|
| Latest `1.0.0-beta.*` | Yes |
| Any earlier beta | No: upgrade to the latest |

## Where the security model is documented

[docs/security.md](docs/security.md) describes how credentials are kept out of
argv and failure reasons, which network connections the binary makes, and how
to verify and install a release offline.

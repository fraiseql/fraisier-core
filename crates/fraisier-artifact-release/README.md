# fraisier-artifact-release

The `release` `ArtifactAdapter` for fraisier. It fetches a release archive over
HTTP, verifies its SHA-256, stages it, and activates it via an atomic symlink
swap.

## Configuration

Read from the `[artifact]` settings table:

```toml
[artifact]
source = "release"
version = "1.2.3"
release_url = "https://example.com/app-{version}.tar.gz"   # {version} substituted
sha256 = "<hex>"                # inline checksum, OR:
checksum_url = "https://example.com/app-{version}.tar.gz.sha256"
staging_dir = "/var/lib/app/staging"   # default <workdir>/.fraisier-staging
active_path = "/var/lib/app/current"   # symlink swapped on activate
```

## Behaviour

- `stage` — download (retrying via `fraisier-adapter-support`), verify the
  SHA-256 (inline `sha256` wins over a fetched `checksum_url`), and write the
  bytes to `<staging_dir>/<version>`. A checksum mismatch aborts before anything
  is staged.
- `activate` — atomically point `active_path` at the staged file (temp symlink +
  `rename`), so `current` never observes a half-updated link.
- `current` — read `active_path`; the artifact id is the link target's file name
  (`None` when nothing is active).

TLS uses rustls (no OpenSSL).

# fraisier-artifact-pull

The **host-pull** `ArtifactAdapter` for [fraisier](https://fraiseql.dev): each host
fetches and activates its *own* release, by shelling out to `curl` / `sha256sum` /
`ln` / `mv` / `readlink` through a `Transport`. With a `Transport::Ssh` those
commands run on the remote host; with the default `Transport::Local` they run
locally.

This is the **simple Linux fleet** strategy — nothing to install on the hosts
beyond standard coreutils + `curl`, and the orchestrator never holds the bytes.

```toml
[artifact]
source = "pull"
version = "1.2.3"
release_url = "https://example.com/app-{version}.tar.gz"
sha256 = "<hex>"                          # or checksum_url = "…"
staging_dir = "/var/lib/app/releases"     # on the host
active_path = "/var/lib/app/current"      # the symlink swapped on activate
```

`stage` downloads to `staging_dir/<version>` and verifies the checksum on the host;
`activate` swaps `active_path` atomically (temp symlink + `mv -T`). `staging_dir`
and `active_path` must be on the same filesystem. `mv -T` / `ln -sfn` are GNU
coreutils — the host-pull strategy targets Linux fleets.

Licensed under MIT OR Apache-2.0.

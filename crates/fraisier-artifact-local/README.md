# fraisier-artifact-local

The [`LocalArtifact`] adapter: deploy an already-built artifact from a local
path. It stages a versioned copy of the path under the staging directory and
activates it with the shared atomic symlink swap — the `release` adapter minus
the download and checksum (PRD §6.3, the `local` artifact source).

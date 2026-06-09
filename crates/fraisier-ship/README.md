# fraisier-ship

Version management and the release workflow behind `fraisier version` and
`fraisier ship`.

- [`locate`] / [`show`] — find and read the project version. `Cargo.toml` takes
  precedence over `pyproject.toml`; both `[package]`/`[workspace.package]` and
  `[project]`/`[tool.poetry]` layouts are understood.
- [`bump`] — increment the version in place with `toml_edit`, preserving the
  file's formatting and comments.
- [`next_version`] — the pure semver bump computation.

A bump increments the requested component, zeroes the lower ones, and clears any
pre-release / build metadata.

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-beta.12](https://github.com/fraiseql/fraisier-core/compare/fraisier-core-v1.0.0-beta.11...fraisier-core-v1.0.0-beta.12) - 2026-09-26

### Fixed

- *(test)* an ambient FRAISIER_* variable no longer fails seven tests

### Other

- release v1.0.0-beta.11

## [1.0.0-beta.11](https://github.com/fraiseql/fraisier-core/compare/fraisier-core-v1.0.0-beta.10...fraisier-core-v1.0.0-beta.11) - 2026-09-26

### Other

- release v1.0.0-beta.11

## [1.0.0-beta.10](https://github.com/fraiseql/fraisier-core/compare/fraisier-core-v1.0.0-beta.9...fraisier-core-v1.0.0-beta.10) - 2026-09-26

### Fixed

- *(core)* a failed verify names its failing checks

### Other

- *(saga)* one credential-redaction helper, reachable from the engine
- *(core)* resolve fraisier-core's six rustdoc warnings

## [1.0.0-beta.9](https://github.com/fraiseql/fraisier-core/compare/fraisier-core-v1.0.0-beta.8...fraisier-core-v1.0.0-beta.9) - 2026-09-20

### Added

- *(core)* VerifyReport says when a verify examined nothing

### Fixed

- *(core)* a verify that examined nothing no longer reads "failed 0 check(s)"

## [1.0.0-beta.8](https://github.com/fraiseql/fraisier-core/compare/fraisier-core-v1.0.0-beta.7...fraisier-core-v1.0.0-beta.8) - 2026-08-08

### Fixed

- *(policy)* stop trusting window_safe from a confiture that cannot classify

### Other

- *(release)* 1.0.0-beta.8
- release v1.0.0-beta.7

### Fixed

- *(policy)* the blue-green baseline now requires the `window_safe` **capability**
  before it reads the verdict. Previously it checked only `preflight`, so an
  adapter that ran the lint without being able to classify every statement could
  answer `Some(true)` and clear the gate. The capability is what distinguishes
  *"nothing unsafe was found"* from *"nothing was recognised"* — two states a
  producer deriving its verdict from an absence of findings cannot tell apart.
  This is not redundant with the existing `None` arm: `None` catches a producer
  that stays silent, the capability catches one that answers confidently about
  statements it never read.

### Added

- *(policy)* `Capabilities::window_safe`, set through the new
  `Capabilities::with_window_safe`. Additive: `Capabilities::new` keeps its
  two-argument signature, and the field defaults to `false` — the safe
  direction, since a gate that needs the capability refuses without it.

## [1.0.0-beta.7](https://github.com/fraiseql/fraisier-core/compare/fraisier-core-v1.0.0-beta.6...fraisier-core-v1.0.0-beta.7) - 2026-08-06

### Added

- *(preview)* the dry-run reads the schema and says what it cannot see
- *(policy)* audit the decision, warn on the silent-deny config, document the gate
- *(policy)* one gate on all three strategies — the window-safety module is gone
- *(policy)* the gate stops a single-host deploy, and an approval hook can unblock it
- *(policy)* one decision function — risk-keyed policy + window-safety baseline
- *(adapter)* parse the confiture change-set, advertise risk_tier honestly
- *(adapter)* carry the schema risk tier across the migration seam

### Other

- *(release)* 1.0.0-beta.7

## [1.0.0-beta.6](https://github.com/fraiseql/fraisier-core/compare/fraisier-core-v1.0.0-beta.5...fraisier-core-v1.0.0-beta.6) - 2026-07-23

### Fixed

- *(command)* run migrations from the staged release directory ([#38](https://github.com/fraiseql/fraisier-core/pull/38))

//! Confiture's exit-code / error-code contract, as one canonical table.
//!
//! Confiture owns this contract — its
//! [`docs/reference/exit-codes.md`](https://github.com/fraiseql/confiture), frozen
//! as a stability contract since issue #146 (integer→meaning never changes; a
//! breaking change needs a major bump). This module mirrors that table into a
//! single place so nothing in this crate re-encodes it ad hoc: [`classify`] maps a
//! `(exit_code, error_code)` pair to a semantic [`ExitClass`], and every consumer
//! is a *thin projection* of that one function —
//! [`ExitClass::to_adapter_kind`] onto the frozen [`AdapterErrorKind`] wire enum,
//! and [`ExitClass::is_retriable`] for the lock-contention retry nuance.
//!
//! Confiture is the single source of truth: it emits the whole table as JSON via
//! `confiture --exit-codes-json` (from its `EXIT_CODE_SEMANTIC_CLASS`). This crate
//! **vendors** that output in [`exit_codes.vendored.json`](./exit_codes.vendored.json)
//! and the tests below diff the Rust table against it (always) and against the
//! *live* command (when a new-enough `confiture` is on `PATH`) — so a drift fails
//! CI here, and confiture's own contract test fails on its side. To adopt a
//! confiture change, regenerate the vendored file:
//!
//! ```sh
//! confiture --exit-codes-json > crates/fraisier-adapter-confiture/src/exit_codes.vendored.json
//! ```
//!
//! The Python adapter (`fraisier` `dbops/confiture_contract.py`) mirrors the same
//! confiture-owned table.

use fraisier_core::adapter_axes::AdapterErrorKind;

/// Confiture's error code for a reachable-but-uninitialised database — no
/// migration ledger (`tb_confiture` absent). It exits 2, and is the one code
/// that identifies "no ledger" when only the structured envelope is in hand.
pub const NO_LEDGER_ERROR_CODE: &str = "PRECON_1001";

/// The semantic class of one confiture process exit — the canonical taxonomy
/// shared with the Python adapter. There is exactly one class per documented
/// exit integer `0..=8`; [`as_str`](Self::as_str) is the cross-repo wire string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitClass {
    /// Exit 0 — success. Present so the table is total; never an error.
    Ok,
    /// Exit 1 — generic / unclassified failure: SQL or hook execution, an
    /// ambiguous-change advisory, `status: pending`, or the `INTERNAL_ERROR`
    /// envelope confiture emits for an unexpected exception.
    InternalError,
    /// Exit 2 — reachable-but-uninitialised database (`PRECON_1001`, no ledger).
    PreconditionFailed,
    /// Exit 3 — database connection failed (host / auth / network unreachable).
    DbUnreachable,
    /// Exit 4 — schema / DDL / build error.
    SchemaError,
    /// Exit 5 — configuration invalid, or a validation / sync / lint failure.
    InvalidConfig,
    /// Exit 6 — lock or connection-pool contention (**retriable**).
    LockContention,
    /// Exit 7 — git / pgGit / grant-accompaniment error.
    GitError,
    /// Exit 8 — irreversible rollback, or inconsistent state after rollback.
    IrreversibleRollback,
}

impl ExitClass {
    /// The stable wire string — identical to the Python adapter's class names.
    /// This is the value the cross-repo fixtures pin. Rust never *emits* it (it
    /// projects to [`AdapterErrorKind`] instead), so it exists only to pin the
    /// contract in the test that mirrors the Python twin — hence `#[cfg(test)]`.
    #[cfg(test)]
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::InternalError => "internal_error",
            Self::PreconditionFailed => "precondition_failed",
            Self::DbUnreachable => "db_unreachable",
            Self::SchemaError => "schema_error",
            Self::InvalidConfig => "invalid_config",
            Self::LockContention => "lock_contention",
            Self::GitError => "git_error",
            Self::IrreversibleRollback => "irreversible_rollback",
        }
    }

    /// Project onto the frozen [`AdapterErrorKind`] — a faithful 1:1 mapping: every
    /// failure class has its own kind on the wire (the enum carries the whole
    /// confiture taxonomy), so nothing is flattened. Lock contention's retry nuance
    /// is *also* carried in the message (see [`is_retriable`](Self::is_retriable)),
    /// on top of the distinct [`LockContention`](AdapterErrorKind::LockContention)
    /// kind.
    pub const fn to_adapter_kind(self) -> AdapterErrorKind {
        match self {
            Self::PreconditionFailed => AdapterErrorKind::PreconditionFailed,
            Self::InvalidConfig => AdapterErrorKind::InvalidConfig,
            Self::DbUnreachable => AdapterErrorKind::DbUnreachable,
            Self::SchemaError => AdapterErrorKind::SchemaError,
            Self::LockContention => AdapterErrorKind::LockContention,
            Self::GitError => AdapterErrorKind::GitError,
            Self::IrreversibleRollback => AdapterErrorKind::IrreversibleRollback,
            Self::InternalError => AdapterErrorKind::InternalError,
            // `Ok` never reaches an error path; map defensively to the generic
            // execution kind so the projection stays total.
            Self::Ok => AdapterErrorKind::Execution,
        }
    }

    /// Whether a failure of this class is worth retrying unchanged. Only lock /
    /// pool contention is — another writer holds the lock; wait and retry.
    pub const fn is_retriable(self) -> bool {
        matches!(self, Self::LockContention)
    }
}

/// Classify a confiture process exit into its semantic [`ExitClass`].
///
/// Keyed on the integer exit code (confiture's frozen `exit-codes.md` table).
/// The error code is consulted for one refinement only: a `PRECON_1001` envelope
/// identifies "no ledger" when the process left **no** exit code (killed by a
/// signal) — so a consumer holding only the structured envelope still classifies
/// it. A present exit code is authoritative and is **never** laundered by the
/// error code: an exit 5 (config invalid) stays [`InvalidConfig`] even if a stray
/// `PRECON_1001` rides along, so a severe failure is never downgraded to a benign
/// precondition. (For a conformant confiture the two always agree — `PRECON_1001`
/// only ever exits 2 — so this matters only for a malformed or skewed producer.)
// Reason: the explicit `Some(1)` arm documents exit 1's meaning against
// exit-codes.md even though it shares the `InternalError` body with the
// catch-all; collapsing it would make the table unreadable as a mirror.
#[allow(clippy::match_same_arms)]
pub fn classify(exit_code: Option<i32>, error_code: Option<&str>) -> ExitClass {
    match exit_code {
        Some(0) => ExitClass::Ok,
        Some(1) => ExitClass::InternalError,
        Some(2) => ExitClass::PreconditionFailed,
        Some(3) => ExitClass::DbUnreachable,
        Some(4) => ExitClass::SchemaError,
        Some(5) => ExitClass::InvalidConfig,
        Some(6) => ExitClass::LockContention,
        Some(7) => ExitClass::GitError,
        Some(8) => ExitClass::IrreversibleRollback,
        // No exit code (killed by signal): unclassifiable by integer. A
        // `PRECON_1001` envelope still names it; otherwise it is internal.
        None if error_code == Some(NO_LEDGER_ERROR_CODE) => ExitClass::PreconditionFailed,
        _ => ExitClass::InternalError,
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, ExitClass, NO_LEDGER_ERROR_CODE};
    use fraisier_core::adapter_axes::AdapterErrorKind;

    /// The canonical `(exit_code, error_code) -> ExitClass` matrix, mirrored from
    /// confiture `docs/reference/exit-codes.md` ("Canonical table", frozen since
    /// #146). The Python adapter's `tests/test_confiture_contract.py` enumerates
    /// the identical matrix against the identical wire strings — a drift on either
    /// side fails CI. Symbolic codes are drawn from that doc's per-exit lists.
    const MATRIX: &[(Option<i32>, Option<&str>, ExitClass)] = &[
        (Some(0), None, ExitClass::Ok),
        (Some(0), Some("MIGR_105"), ExitClass::Ok),
        (Some(1), None, ExitClass::InternalError),
        (Some(1), Some("INTERNAL_ERROR"), ExitClass::InternalError),
        (Some(1), Some("SQL_001"), ExitClass::InternalError),
        (Some(2), None, ExitClass::PreconditionFailed),
        (Some(2), Some("PRECON_1001"), ExitClass::PreconditionFailed),
        (Some(3), None, ExitClass::DbUnreachable),
        (Some(3), Some("CONFIG_006"), ExitClass::DbUnreachable),
        (Some(4), None, ExitClass::SchemaError),
        (Some(4), Some("SCHEMA_001"), ExitClass::SchemaError),
        (Some(5), None, ExitClass::InvalidConfig),
        (Some(5), Some("CONFIG_010"), ExitClass::InvalidConfig),
        (Some(5), Some("VALID_001"), ExitClass::InvalidConfig),
        (Some(6), None, ExitClass::LockContention),
        (Some(6), Some("LOCK_1300"), ExitClass::LockContention),
        (Some(7), None, ExitClass::GitError),
        (Some(7), Some("GIT_001"), ExitClass::GitError),
        (Some(8), None, ExitClass::IrreversibleRollback),
        (
            Some(8),
            Some("ROLLBACK_600"),
            ExitClass::IrreversibleRollback,
        ),
        // Refinement: a present exit code is authoritative and is never laundered
        // by the error code — exit 5 stays InvalidConfig even under a stray
        // PRECON_1001, so a real config error is never downgraded.
        (Some(5), Some("PRECON_1001"), ExitClass::InvalidConfig),
        // ...but with no exit code at all (signal), a PRECON_1001 envelope still
        // identifies "no ledger"; anything else is internal.
        (None, None, ExitClass::InternalError),
        (None, Some("PRECON_1001"), ExitClass::PreconditionFailed),
        (None, Some("LOCK_1300"), ExitClass::InternalError),
        // An exit code outside the documented 0..=8 universe is internal.
        (Some(9), None, ExitClass::InternalError),
    ];

    #[test]
    fn classify_covers_the_confiture_exit_code_matrix() {
        for (code, error_code, expected) in MATRIX {
            assert_eq!(
                classify(*code, *error_code),
                *expected,
                "classify({code:?}, {error_code:?})"
            );
        }
    }

    #[test]
    fn projection_to_adapter_kind_is_faithful_one_to_one() {
        // Every failure class maps to its own wire kind (nothing flattened).
        let pairs = [
            (
                ExitClass::PreconditionFailed,
                AdapterErrorKind::PreconditionFailed,
            ),
            (ExitClass::InvalidConfig, AdapterErrorKind::InvalidConfig),
            (ExitClass::DbUnreachable, AdapterErrorKind::DbUnreachable),
            (ExitClass::SchemaError, AdapterErrorKind::SchemaError),
            (ExitClass::LockContention, AdapterErrorKind::LockContention),
            (ExitClass::GitError, AdapterErrorKind::GitError),
            (
                ExitClass::IrreversibleRollback,
                AdapterErrorKind::IrreversibleRollback,
            ),
            (ExitClass::InternalError, AdapterErrorKind::InternalError),
        ];
        for (class, kind) in pairs {
            assert_eq!(class.to_adapter_kind(), kind, "{class:?}");
            // The class and its wire kind share the exact same wire string.
            assert_eq!(class.as_str(), kind.as_str(), "{class:?} wire string");
        }
        // `Ok` is never an error; it maps defensively to the generic kind.
        assert_eq!(ExitClass::Ok.to_adapter_kind(), AdapterErrorKind::Execution);
    }

    #[test]
    fn only_lock_contention_is_retriable() {
        assert!(ExitClass::LockContention.is_retriable());
        for class in [
            ExitClass::Ok,
            ExitClass::InternalError,
            ExitClass::PreconditionFailed,
            ExitClass::DbUnreachable,
            ExitClass::SchemaError,
            ExitClass::InvalidConfig,
            ExitClass::GitError,
            ExitClass::IrreversibleRollback,
        ] {
            assert!(!class.is_retriable(), "{class:?} must not be retriable");
        }
    }

    #[test]
    fn wire_strings_are_the_cross_repo_contract() {
        // These exact strings are what the Python twin pins. Order is the exit
        // integer 0..=8 so a reviewer can read it against exit-codes.md.
        let expected = [
            (ExitClass::Ok, "ok"),
            (ExitClass::InternalError, "internal_error"),
            (ExitClass::PreconditionFailed, "precondition_failed"),
            (ExitClass::DbUnreachable, "db_unreachable"),
            (ExitClass::SchemaError, "schema_error"),
            (ExitClass::InvalidConfig, "invalid_config"),
            (ExitClass::LockContention, "lock_contention"),
            (ExitClass::GitError, "git_error"),
            (ExitClass::IrreversibleRollback, "irreversible_rollback"),
        ];
        for (class, wire) in expected {
            assert_eq!(class.as_str(), wire);
        }
    }

    #[test]
    fn no_ledger_error_code_is_precon_1001() {
        // Pinned so a rename in confiture (a breaking change on its side) is
        // caught here rather than silently misclassifying "no ledger".
        assert_eq!(NO_LEDGER_ERROR_CODE, "PRECON_1001");
    }

    // The confiture-owned contract, vendored verbatim from `confiture
    // --exit-codes-json`. Regenerate with the command in the module docs.
    const VENDORED_JSON: &str = include_str!("exit_codes.vendored.json");

    /// Extract the frozen `{exit_int: class}` map from a `--exit-codes-json` doc,
    /// ignoring the informational `meaning`/`symbolic_codes` (which grow additively).
    fn class_map(doc: &serde_json::Value) -> std::collections::BTreeMap<i32, String> {
        doc["exit_codes"]
            .as_object()
            .expect("exit_codes object")
            .iter()
            .map(|(code, entry)| {
                (
                    code.parse::<i32>().expect("exit code integer"),
                    entry["class"].as_str().expect("class string").to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn rust_table_matches_the_vendored_confiture_contract() {
        // The always-on drift guard: the Rust `classify`/`ExitClass` table must
        // equal the vendored confiture contract, field for field.
        let doc: serde_json::Value =
            serde_json::from_str(VENDORED_JSON).expect("vendored json parses");
        assert_eq!(
            doc["no_ledger_error_code"].as_str(),
            Some(NO_LEDGER_ERROR_CODE)
        );
        // Every vendored exit code classifies to the vendored class name.
        for (code, class) in class_map(&doc) {
            assert_eq!(
                classify(Some(code), None).as_str(),
                class,
                "exit {code} disagrees with the vendored confiture contract"
            );
        }
        // The Rust taxonomy's wire strings are exactly the vendored `classes` set.
        let vendored_classes: std::collections::BTreeSet<&str> = doc["classes"]
            .as_array()
            .expect("classes array")
            .iter()
            .map(|v| v.as_str().expect("class string"))
            .collect();
        let rust_classes: std::collections::BTreeSet<&str> = [
            ExitClass::Ok,
            ExitClass::InternalError,
            ExitClass::PreconditionFailed,
            ExitClass::DbUnreachable,
            ExitClass::SchemaError,
            ExitClass::InvalidConfig,
            ExitClass::LockContention,
            ExitClass::GitError,
            ExitClass::IrreversibleRollback,
        ]
        .iter()
        .map(|c| c.as_str())
        .collect();
        assert_eq!(rust_classes, vendored_classes);
    }

    #[test]
    fn vendored_contract_matches_live_confiture_when_available() {
        // The cross-repo freshness check: when a new-enough `confiture` is on PATH
        // (or FRAISIER_CONFITURE_BIN), the vendored file must still equal what it
        // emits. Skips otherwise (an older confiture lacks the flag; CI without
        // confiture cannot run it) — the test above is the always-on guard.
        let program = std::env::var_os("FRAISIER_CONFITURE_BIN")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| std::ffi::OsString::from("confiture"));
        let output = match std::process::Command::new(&program)
            .arg("--exit-codes-json")
            .output()
        {
            Ok(output) if output.status.success() => output,
            _ => {
                eprintln!("skip: `confiture --exit-codes-json` unavailable (old or absent)");
                return;
            }
        };
        let Ok(live) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
            eprintln!("skip: confiture output is not JSON (confiture too old?)");
            return;
        };
        let vendored: serde_json::Value =
            serde_json::from_str(VENDORED_JSON).expect("vendored json parses");
        assert_eq!(
            live["no_ledger_error_code"], vendored["no_ledger_error_code"],
            "vendored exit_codes.vendored.json is stale (no_ledger); regenerate it"
        );
        assert_eq!(
            class_map(&live),
            class_map(&vendored),
            "vendored exit_codes.vendored.json is stale; regenerate: \
             confiture --exit-codes-json > crates/fraisier-adapter-confiture/src/exit_codes.vendored.json"
        );
    }
}

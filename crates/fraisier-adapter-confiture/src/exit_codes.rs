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
//! **vendors** that output in [`exit_codes.vendored.json`](./exit_codes.vendored.json).
//! The tests below diff the Rust table against that file, and the file against what
//! the pinned confiture emits — the whole document, and never a skip: the pin lives
//! in `tools/confiture-requirements.txt` and CI installs it before the gate, so a
//! drift fails CI here and confiture's own contract test fails on its side. To adopt
//! a confiture change, bump that pin and regenerate the vendored file from the same
//! release in one commit — the freshness test fails on either half alone:
//!
//! ```sh
//! uv venv --python 3.11 /tmp/confiture
//! uv pip install --python /tmp/confiture/bin/python -r tools/confiture-requirements.txt
//! /tmp/confiture/bin/confiture --exit-codes-json \
//!   > crates/fraisier-adapter-confiture/src/exit_codes.vendored.json
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
    /// Exit 1 — generic / unclassified failure: SQL or hook execution,
    /// `status: pending`, or the `INTERNAL_ERROR` envelope confiture emits for an
    /// unexpected non-`ConfiturError` exception.
    InternalError,
    /// Exit 2 — reachable-but-uninitialised database (`PRECON_1001`, no ledger).
    PreconditionFailed,
    /// Exit 3 — database connection failed (host / auth / network unreachable).
    DbUnreachable,
    /// Exit 4 — schema / DDL / build error.
    SchemaError,
    /// Exit 5 — configuration invalid, or a validation / sync / lint failure.
    InvalidConfig,
    /// Exit 6 — lock contention: another writer holds the lock (**retriable**).
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

    /// Whether a failure of this class is worth retrying unchanged. Only lock
    /// contention is — another writer holds the lock; wait and retry.
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

    /// Every [`ExitClass`], in exit-integer order. Rust cannot enumerate an enum, so this
    /// is the one place the variants are listed; the tests below check it against the
    /// vendored contract's `classes` rather than against a second copy of the wire strings.
    const ALL_CLASSES: [ExitClass; 9] = [
        ExitClass::Ok,
        ExitClass::InternalError,
        ExitClass::PreconditionFailed,
        ExitClass::DbUnreachable,
        ExitClass::SchemaError,
        ExitClass::InvalidConfig,
        ExitClass::LockContention,
        ExitClass::GitError,
        ExitClass::IrreversibleRollback,
    ];

    /// The canonical `(exit_code, error_code) -> ExitClass` matrix, mirrored from
    /// confiture `docs/reference/exit-codes.md` ("Canonical table", frozen since
    /// #146). The Python adapter's `tests/test_confiture_contract.py` enumerates
    /// the identical matrix against the identical wire strings — a drift on either
    /// side fails CI. Symbolic codes are drawn from that doc's per-exit lists.
    const MATRIX: &[(Option<i32>, Option<&str>, ExitClass)] = &[
        (Some(0), None, ExitClass::Ok),
        (Some(0), Some("MIGR_101"), ExitClass::Ok),
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
        for class in ALL_CLASSES {
            assert_eq!(
                class.is_retriable(),
                class == ExitClass::LockContention,
                "{class:?} retriability"
            );
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

    /// Extract the `{exit_int: class}` map from a `--exit-codes-json` doc — the part of the
    /// contract confiture froze, and all this projection may assume. `meaning` and
    /// `symbolic_codes` are deliberately dropped here because the Rust table does not
    /// encode them; they are held to the tool by
    /// [`vendored_contract_equals_the_pinned_confitures_exit_codes_json`], which compares
    /// the documents whole. (They were once called informational and additive. They are
    /// neither: confiture 1.19.0 *removed* codes from six exits, #63.)
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
        // The Rust taxonomy's wire strings are exactly the vendored `classes` set — this is
        // what pins `as_str`, so the strings live in the vendored file and nowhere else.
        let vendored_classes: std::collections::BTreeSet<&str> = doc["classes"]
            .as_array()
            .expect("classes array")
            .iter()
            .map(|v| v.as_str().expect("class string"))
            .collect();
        let rust_classes: std::collections::BTreeSet<&str> =
            ALL_CLASSES.iter().map(|c| c.as_str()).collect();
        assert_eq!(rust_classes, vendored_classes);
    }

    #[test]
    fn the_matrix_names_symbolic_codes_the_vendored_contract_lists() {
        // MATRIX quotes confiture's symbolic codes as examples, and an example can go stale
        // without any test noticing: it carried `MIGR_105` under exit 0 long after confiture
        // dropped that code entirely (#63). Every example is therefore held to the vendored
        // per-exit list, with two rows exempt by construction:
        //   - exit 1 / `INTERNAL_ERROR` — the code confiture stamps on an unexpected
        //     non-`ConfiturError`; real, and in no per-exit list;
        //   - exit 5 / `PRECON_1001` — the deliberate skew row proving a present exit code
        //     is never laundered by the error code, so the code belongs to exit 2 on purpose.
        const EXEMPT: &[(i32, &str)] = &[(1, "INTERNAL_ERROR"), (5, "PRECON_1001")];
        let doc: serde_json::Value =
            serde_json::from_str(VENDORED_JSON).expect("vendored json parses");
        for (exit, code) in MATRIX
            .iter()
            .filter_map(|(exit, code, _)| exit.zip(*code))
            .filter(|pair| !EXEMPT.contains(pair))
        {
            let listed = doc["exit_codes"][exit.to_string()]["symbolic_codes"]
                .as_array()
                .unwrap_or_else(|| panic!("exit {exit} has no symbolic_codes in the contract"))
                .iter()
                .any(|listed| listed.as_str() == Some(code));
            assert!(
                listed,
                "the matrix names {code} under exit {exit}, and the vendored confiture \
                 contract does not list it there"
            );
        }
    }

    /// The confiture release the exit-code contract is measured against, read out of
    /// `tools/confiture-requirements.txt` so the pin has exactly one home. The test below
    /// asserts the tool it runs reports *this* version, which couples a pin bump to a
    /// regeneration of the vendored file in the same commit — and makes "too old to have
    /// the flag" a named failure instead of a silent pass.
    fn pinned_confiture_version() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("the workspace root is two levels above crates/<name>")
            .join("tools/confiture-requirements.txt");
        let pins = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("reading the confiture pin {}: {error}", path.display())
        });
        pins.lines()
            .find_map(|line| line.trim().strip_prefix("fraiseql-confiture=="))
            .unwrap_or_else(|| {
                panic!(
                    "{} names no `fraiseql-confiture==<version>`",
                    path.display()
                )
            })
            .to_owned()
    }

    /// The confiture to measure: `FRAISIER_CONFITURE_BIN` when set (CI points it at the
    /// venv built from the pin), otherwise `confiture` on `PATH` — the same override the
    /// adapter itself honours.
    fn confiture_program() -> std::ffi::OsString {
        std::env::var_os("FRAISIER_CONFITURE_BIN")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| std::ffi::OsString::from("confiture"))
    }

    /// The paths at which two `--exit-codes-json` documents differ, one per line, so the
    /// failure below names what drifted instead of printing two 3 kB documents and leaving
    /// the reader to find it. Only a hint: the assertion itself compares the documents
    /// whole, so a difference this walker cannot localise still fails.
    fn describe_drift(live: &serde_json::Value, vendored: &serde_json::Value) -> String {
        /// The union of two objects' field names, so a field missing on one side is still
        /// reported rather than skipped.
        fn names(
            left: Option<&serde_json::Value>,
            right: Option<&serde_json::Value>,
        ) -> std::collections::BTreeSet<String> {
            [left, right]
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_object)
                .flat_map(|object| object.keys().cloned())
                .collect()
        }
        fn show(value: Option<&serde_json::Value>) -> String {
            value.map_or_else(|| "absent".to_owned(), ToString::to_string)
        }

        let mut lines = Vec::new();
        for key in names(Some(live), Some(vendored)) {
            let (live_value, vendored_value) = (live.get(&key), vendored.get(&key));
            if live_value == vendored_value {
                continue;
            }
            if key != "exit_codes" {
                lines.push(format!(
                    "  {key}: vendored {}, confiture {}",
                    show(vendored_value),
                    show(live_value)
                ));
                continue;
            }
            for exit in names(live_value, vendored_value) {
                let live_exit = live_value.and_then(|table| table.get(&exit));
                let vendored_exit = vendored_value.and_then(|table| table.get(&exit));
                if live_exit == vendored_exit {
                    continue;
                }
                for field in names(live_exit, vendored_exit) {
                    let live_field = live_exit.and_then(|entry| entry.get(&field));
                    let vendored_field = vendored_exit.and_then(|entry| entry.get(&field));
                    if live_field != vendored_field {
                        lines.push(format!(
                            "  exit {exit} {field}: vendored {}, confiture {}",
                            show(vendored_field),
                            show(live_field)
                        ));
                    }
                }
            }
        }
        if lines.is_empty() {
            "  (the documents differ in a way this walker did not localise)".to_owned()
        } else {
            lines.join("\n")
        }
    }

    /// Quoted in every failure below, so a red checkout is three commands from green. It
    /// puts confiture on `PATH` rather than in `FRAISIER_CONFITURE_BIN`, which stays
    /// honoured: an ambient `FRAISIER_*` variable reddens seven unrelated approval tests
    /// (#64), so the hint must not hand anyone that trap.
    fn install_hint(pin: &str) -> String {
        format!(
            "the exit-code contract is measured against confiture {pin}. Install it:\n  \
             uv venv --python 3.11 /tmp/confiture\n  \
             uv pip install --python /tmp/confiture/bin/python -r \
             tools/confiture-requirements.txt\n  \
             PATH=/tmp/confiture/bin:$PATH cargo xtask ci\n\
             (CI installs the same pin and puts it on PATH before the gate.)"
        )
    }

    #[test]
    fn vendored_contract_equals_the_pinned_confitures_exit_codes_json() {
        // The cross-repo freshness check, and the one that has to compare the WHOLE
        // document: the reduced integer→class map above stayed identical through eight
        // drifted `symbolic_codes` lists and two rewritten `meaning` strings (#63). Its
        // predecessor also `return`ed with a printed `skip:` when confiture was absent,
        // and CI installed no confiture, so it had never once run. A missing or unpinned
        // confiture is therefore a failure here, never a skip.
        let pin = pinned_confiture_version();
        let program = confiture_program();
        let shown = program.to_string_lossy().into_owned();

        let version = match std::process::Command::new(&program)
            .arg("--version")
            .output()
        {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned(),
            other => panic!(
                "`{shown} --version` did not answer ({other:?}).\n{}",
                install_hint(&pin)
            ),
        };
        // `confiture --version` opens with `confiture version <semver>`; its later lines
        // report the parser build and native extension, which are the machine's rather
        // than the release's, so only the first line is the contract.
        assert_eq!(
            version,
            format!("confiture version {pin}"),
            "this is a different confiture, so any diff below would be the wrong \
             release's.\n{}",
            install_hint(&pin)
        );

        let output = std::process::Command::new(&program)
            .arg("--exit-codes-json")
            .output()
            .unwrap_or_else(|error| panic!("running `{shown} --exit-codes-json`: {error}"));
        assert!(
            output.status.success(),
            "`{shown} --exit-codes-json` exited {:?}",
            output.status.code()
        );
        let live: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("confiture emits JSON");
        let vendored: serde_json::Value =
            serde_json::from_str(VENDORED_JSON).expect("vendored json parses");
        assert_eq!(
            live,
            vendored,
            "exit_codes.vendored.json is not what confiture {pin} emits. It drifts at:\n\
             {}\nRegenerate it:\n  \
             {shown} --exit-codes-json > \
             crates/fraisier-adapter-confiture/src/exit_codes.vendored.json",
            describe_drift(&live, &vendored)
        );
    }
}

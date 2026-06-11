//! Phase 1 fixture test: `scripts/perf-scan-stub.sh` reproduces the pinned
//! fraiseql v2.6.0 perf `regression-scan` seam (exit-code contract + `--json`
//! shape) so the command health adapter and its rollback e2e (Phases 2–3) can be
//! built and CI-tested with no live database and no released `fraiseql`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// The committed stub, located relative to the workspace root.
fn stub() -> PathBuf {
    // crates/fraisier-cli → crates → <workspace root>.
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/fraisier-cli");
    workspace.join("scripts/perf-scan-stub.sh")
}

/// Run the stub with `args` and extra `env`, returning its exit code and stdout.
fn run(args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
    let mut command = Command::new(stub());
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("spawn perf-scan-stub");
    let code = output
        .status
        .code()
        .expect("stub exits via a status code, not a signal");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    (code, stdout)
}

#[test]
fn default_scan_is_healthy_exit_zero_with_no_findings() {
    let (code, stdout) = run(&["--fail-on-regression", "--json"], &[]);
    assert_eq!(
        code, 0,
        "no regression → exit 0 even under --fail-on-regression"
    );
    let report: Value = serde_json::from_str(&stdout).expect("valid --json report");
    assert!(
        report["findings"]
            .as_array()
            .expect("findings array")
            .is_empty(),
        "no findings by default",
    );
    assert_eq!(report["summary"]["regressions"].as_u64(), Some(0));
}

#[test]
fn regression_gates_only_under_fail_on_regression() {
    // Regression present AND --fail-on-regression → the gate fails (exit 1).
    let (code, stdout) = run(&["--fail-on-regression", "--json"], &[("REGRESS", "1")]);
    assert_eq!(code, 1, "regression + flag → exit 1");
    let report: Value = serde_json::from_str(&stdout).expect("valid --json report");
    let findings = report["findings"].as_array().expect("findings array");
    assert_eq!(findings.len(), 1, "exactly one simulated regression");
    assert_eq!(findings[0]["object_type"].as_str(), Some("order"));
    assert_eq!(findings[0]["modification_type"].as_str(), Some("UPDATE"));
    assert_eq!(report["summary"]["regressions"].as_u64(), Some(1));

    // Regression present but NO flag → it is a report, not a gate: exit 0.
    let (code, _) = run(&["--json"], &[("REGRESS", "1")]);
    assert_eq!(code, 0, "a regression without the flag still exits 0");
}

#[test]
fn operational_error_is_distinct_nonzero_with_no_report() {
    let (code, stdout) = run(&["--fail-on-regression", "--json"], &[("FAIL_OP", "1")]);
    assert_ne!(code, 0, "operational error → non-zero");
    assert_ne!(
        code, 1,
        "and distinguishable from the regression gate (exit 1)"
    );
    assert!(
        stdout.trim().is_empty(),
        "an operational error prints no report on stdout",
    );
}

#[test]
fn json_shape_matches_the_pinned_v2_6_0_contract() {
    let (_, stdout) = run(&["--json"], &[("REGRESS", "1")]);
    let report: Value = serde_json::from_str(&stdout).expect("valid --json report");

    for key in ["findings", "skipped", "summary"] {
        assert!(report.get(key).is_some(), "missing top-level `{key}`");
    }
    for key in [
        "groups_analyzed",
        "regressions",
        "total_samples",
        "excluded_samples",
    ] {
        assert!(
            report["summary"].get(key).is_some(),
            "missing summary.`{key}`",
        );
    }
    let finding = &report["findings"][0];
    for key in [
        "object_type",
        "modification_type",
        "baseline_p50",
        "baseline_p95",
        "recent_p50",
        "recent_p95",
        "pct_change",
        "baseline_samples",
        "recent_samples",
    ] {
        assert!(finding.get(key).is_some(), "missing findings[].`{key}`");
    }
    assert!(
        finding["recent_p50"].is_number(),
        "latencies are JSON numbers, not strings",
    );
}

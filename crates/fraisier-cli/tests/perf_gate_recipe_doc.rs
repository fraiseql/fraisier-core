//! Phase 1: the documented Tier-0 perf-gate recipe in
//! `docs/perf-regression-gate.md` must actually parse as a fraisier config, so the
//! published example cannot drift into a non-parsing state.

use std::path::Path;

use fraisier_config::DeployConfig;

/// The body of the first fenced toml code block in a markdown string.
fn first_toml_block(markdown: &str) -> &str {
    markdown
        .split_once("```toml")
        .expect("a ```toml block in the doc")
        .1
        .split_once("```")
        .expect("the toml block is closed")
        .0
        .trim()
}

#[test]
fn documented_perf_gate_recipe_parses() {
    // crates/fraisier-cli → crates → <workspace root>.
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/fraisier-cli");
    let doc = std::fs::read_to_string(workspace.join("docs/perf-regression-gate.md"))
        .expect("read docs/perf-regression-gate.md");

    let config =
        DeployConfig::from_toml_str(first_toml_block(&doc)).expect("the documented recipe parses");

    let migration = config.migration.expect("[migration] section");
    assert_eq!(migration.adapter.as_deref(), Some("command"));
    assert_eq!(
        migration.settings["commands"]["verify"].as_str(),
        Some("fraiseql perf regression-scan --fail-on-regression"),
        "the documented gate is the perf scan, wired into the verify hook",
    );
}

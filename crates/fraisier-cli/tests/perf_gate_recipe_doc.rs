//! The documented perf-gate recipes in `docs/perf-regression-gate.md` must
//! actually parse as fraisier configs, so the published examples cannot drift
//! into a non-parsing state: the recommended `command` health adapter and the
//! fallback command-migration `verify` hook.

use std::path::Path;

use fraisier_config::DeployConfig;

/// The body of every fenced toml code block in a markdown string, in order.
fn toml_blocks(markdown: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = markdown;
    while let Some((_, after)) = rest.split_once("```toml") {
        let (body, tail) = after.split_once("```").expect("the toml block is closed");
        blocks.push(body.trim());
        rest = tail;
    }
    blocks
}

#[test]
fn documented_perf_gate_recipes_parse() {
    // crates/fraisier-cli → crates → <workspace root>.
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/fraisier-cli");
    let doc = std::fs::read_to_string(workspace.join("docs/perf-regression-gate.md"))
        .expect("read docs/perf-regression-gate.md");

    let blocks = toml_blocks(&doc);
    assert!(blocks.len() >= 2, "the doc carries both recipes");

    // Recommended recipe: the command health adapter (migration-agnostic).
    let recommended =
        DeployConfig::from_toml_str(blocks[0]).expect("the recommended recipe parses");
    let health = recommended.health.expect("[health] section");
    assert_eq!(health.adapter.as_deref(), Some("command"));
    assert_eq!(
        health.command.as_deref(),
        Some("fraiseql perf regression-scan --fail-on-regression"),
        "the recommended gate is the perf scan, wired into the health adapter",
    );

    // Fallback recipe: the command-migration verify hook.
    let fallback = DeployConfig::from_toml_str(blocks[1]).expect("the fallback recipe parses");
    let migration = fallback.migration.expect("[migration] section");
    assert_eq!(migration.adapter.as_deref(), Some("command"));
    assert_eq!(
        migration.settings["commands"]["verify"].as_str(),
        Some("fraiseql perf regression-scan --fail-on-regression"),
        "the fallback gate is the perf scan, wired into the verify hook",
    );
}

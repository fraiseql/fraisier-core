//! The recipe published in `docs/schema-risk-policy.md` must actually parse as a
//! fraisier config, and must resolve to the policy it claims to describe.
//!
//! A published `[policy]` block that no longer means what its comments say is
//! worse than no example: an operator copies it, and the gate they get is not
//! the gate they read about.

use std::path::Path;

use fraisier_config::DeployConfig;
use fraisier_core::adapter_axes::RiskTier;
use fraisier_core::policy::{PolicyAction, UnclassifiedAction};

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

/// The operator guide, read from the workspace root.
fn guide() -> String {
    // crates/fraisier-cli → crates → <workspace root>.
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/fraisier-cli");
    std::fs::read_to_string(workspace.join("docs/schema-risk-policy.md"))
        .expect("read docs/schema-risk-policy.md")
}

#[test]
fn the_documented_policy_recipe_resolves_to_the_policy_it_describes() {
    let doc = guide();
    let blocks = toml_blocks(&doc);
    let config = DeployConfig::from_toml_str(blocks[0]).expect("the published recipe parses");
    let policy = config
        .policy
        .as_ref()
        .expect("[policy] section")
        .resolve()
        .with_approval_hook(true);

    // Each commented claim in the recipe, checked against what it resolves to.
    for tier in [RiskTier::Additive, RiskTier::Reversible] {
        assert_eq!(policy.actions.get(&tier), Some(&PolicyAction::AutoApply));
    }
    for tier in [
        RiskTier::LockRisky,
        RiskTier::Destructive,
        RiskTier::Irreversible,
    ] {
        assert_eq!(
            policy.actions.get(&tier),
            Some(&PolicyAction::RequireApproval)
        );
    }
    assert_eq!(policy.unclassified, UnclassifiedAction::Deny);
    assert!(policy.has_approval_hook);
}

/// Whether `doc` mentions `flag` as a whole word.
///
/// A substring test would accept `--fail-on-blocked` as evidence that
/// `--fail-on-block` is documented, which is exactly the rename this guards
/// against.
fn mentions_flag(doc: &str, flag: &str) -> bool {
    doc.split(|c: char| c.is_whitespace() || "`,.()".contains(c))
        .any(|token| token == flag)
}

#[test]
fn the_guide_documents_the_flags_a_pipeline_depends_on() {
    // These are the published contract for a CI job: the offline escape hatch
    // and the gate. Renaming either without touching the guide leaves operators
    // reading instructions that no longer work.
    let doc = guide();
    for flag in ["--skip-preflight", "--fail-on-block", "--dry-run"] {
        assert!(
            mentions_flag(&doc, flag),
            "the guide does not mention {flag}"
        );
    }
    // The distinction the whole preview exists to preserve.
    assert!(
        doc.contains("Risk is unknown, not zero."),
        "the guide drops the phrase the render is built around"
    );
}

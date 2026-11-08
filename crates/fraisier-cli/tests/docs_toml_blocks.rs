//! Every fenced toml block in the README and `docs/` parses as the file it
//! shows: a `fraisier.toml` (or a fragment of one), or a stack file.
//!
//! Both refuse an unknown key, so a key the docs misspell, or one a release
//! renamed, fails here instead of in the operator's first `fraisier validate`.

use std::path::{Path, PathBuf};

use fraisier_config::stack::StackFile;
use fraisier_config::DeployConfig;

/// The workspace root: `crates/fraisier-cli` → `crates` → root.
fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above crates/fraisier-cli")
        .to_path_buf()
}

/// Each fenced toml block in `markdown`, with the line its body starts on.
fn toml_blocks(markdown: &str) -> Vec<(usize, String)> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, String)> = None;
    for (index, line) in markdown.lines().enumerate() {
        let fence = line.trim_start();
        match open.as_mut() {
            None if fence == "```toml" => open = Some((index + 2, String::new())),
            Some(_) if fence == "```" => blocks.extend(open.take()),
            Some((_, body)) => {
                body.push_str(line);
                body.push('\n');
            }
            None => {}
        }
    }
    assert!(open.is_none(), "a toml block is never closed");
    blocks
}

fn parses(body: &str) -> Result<(), String> {
    if body.trim_start().starts_with("[stack") {
        StackFile::from_toml_str(body)
            .map(|_| ())
            .map_err(|error| error.0.join("; "))
    } else {
        DeployConfig::from_toml_str(body)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

#[test]
fn every_toml_block_in_the_readme_and_docs_parses() {
    let root = workspace();
    let mut files = vec![root.join("README.md")];
    let mut docs: Vec<PathBuf> = std::fs::read_dir(root.join("docs"))
        .expect("read docs/")
        .map(|entry| entry.expect("a docs/ entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    docs.sort();
    files.extend(docs);

    let mut checked = 0;
    let mut failures = Vec::new();
    for file in &files {
        let markdown = std::fs::read_to_string(file).expect("read a markdown file");
        let shown = file.strip_prefix(&root).unwrap_or(file).display();
        for (line, body) in toml_blocks(&markdown) {
            checked += 1;
            if let Err(error) = parses(&body) {
                failures.push(format!("{shown}:{line}: {error}"));
            }
        }
    }
    // The README alone has six: a scanner that finds none is broken, not clean.
    assert!(checked >= 20, "found only {checked} toml blocks");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

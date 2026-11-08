//! Production code reads a database URL with `PgConn::resolve`, never a bare
//! `PgConn::parse`: only `resolve` refuses an ambient TLS or GSS policy the URL
//! does not pin, which the database child would otherwise lose without a word.

use std::path::{Path, PathBuf};

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_production_code_parses_a_dsn_without_resolving_it() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates dir") {
        let src = entry.expect("entry").path().join("src");
        if src.is_dir() {
            sources(&src, &mut files);
        }
    }
    let mut bare = Vec::new();
    for file in files {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.ends_with("tests.rs") || file.ends_with("fraisier-db/src/lib.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).expect("utf8 source");
        let production = text.split("#[cfg(test)]").next().unwrap_or_default();
        for (index, line) in production.lines().enumerate() {
            if line.contains("PgConn::parse(") {
                bare.push(format!("{}:{}", file.display(), index + 1));
            }
        }
    }
    assert!(bare.is_empty(), "use PgConn::resolve: {bare:#?}");
}

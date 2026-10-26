//! Workspace task runner — the single source of truth for the CI gate.
//!
//! `cargo xtask ci` runs the exact checks CI runs, so "green locally" and "green
//! in CI" are the *same command* rather than two lists that drift apart. CI
//! invokes `cargo xtask ci` too, which makes the parity structural. Each check is
//! also runnable on its own (`cargo xtask toolchain` / `fmt` / `lint` / `doc` /
//! `test` / `deny` / `shellcheck`), and `cargo xtask dist` cross-builds the static
//! musl binary.
//!
//! This crate deliberately has **zero dependencies** (std only): the task runner
//! that guards the supply chain must not add to it, and a dependency-free tool
//! never breaks because of upstream churn.

use std::fmt::Write as _;
use std::process::{Command, ExitCode, Stdio};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((task, rest)) = args.split_first() else {
        usage();
        return ExitCode::FAILURE;
    };

    let result = match task.as_str() {
        "ci" => ci(),
        "toolchain" => toolchain(),
        "fmt" => fmt(),
        "lint" => lint(),
        "doc" => doc(),
        "test" => test(),
        "deny" => deny(),
        "shellcheck" => shellcheck(),
        "dist" => dist(rest),
        "help" | "--help" | "-h" => {
            usage();
            Ok(())
        }
        other => Err(format!(
            "unknown task `{other}` — run `cargo xtask help` for the task list"
        )),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("\n\x1b[1;31mxtask: {message}\x1b[0m");
            ExitCode::FAILURE
        }
    }
}

/// The full gate, in the order CI runs it. Fails fast on the first broken check.
fn ci() -> Result<(), String> {
    toolchain()?;
    fmt()?;
    lint()?;
    doc()?;
    test()?;
    run("cargo", &["build", "--release"])?;
    deny()?;
    shellcheck()?;
    println!("\n\x1b[1;32m✓ all gate checks passed\x1b[0m");
    Ok(())
}

/// The toolchain is pinned to an exact release, and the MSRV is that release.
///
/// A floating `channel = "stable"` let Rust 1.99's new clippy lints turn an
/// untouched `main` red (#80). An exact pin moves only in a commit that carries
/// its own lint fixes. The MSRV is the pinned version because that is the only
/// version the gate ever builds with: a lower `rust-version` would be a claim
/// nothing tests.
fn toolchain() -> Result<(), String> {
    let read = |path: &str| {
        std::fs::read_to_string(path).map_err(|error| format!("reading {path}: {error}"))
    };
    let channel = toml_string(&read("rust-toolchain.toml")?, "channel")
        .ok_or("rust-toolchain.toml has no `channel = \"…\"` line")?;
    let rust_version = toml_string(&read("Cargo.toml")?, "rust-version")
        .ok_or("Cargo.toml has no `rust-version = \"…\"` line")?;
    check_toolchain(&channel, &rust_version)?;
    println!("\x1b[1;36m$ toolchain pinned to {channel}, rust-version {rust_version}\x1b[0m");
    Ok(())
}

/// `channel` must be an exact `X.Y.Z`, and `rust-version` must be its `X.Y`.
fn check_toolchain(channel: &str, rust_version: &str) -> Result<(), String> {
    let parts: Vec<&str> = channel.split('.').collect();
    let exact = parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if !exact {
        return Err(format!(
            "rust-toolchain.toml pins `channel = \"{channel}\"`, which floats; pin an exact \
             release such as \"1.99.0\""
        ));
    }
    let floor = format!("{}.{}", parts[0], parts[1]);
    if rust_version != floor {
        return Err(format!(
            "Cargo.toml declares `rust-version = \"{rust_version}\"` but the toolchain is \
             {channel}; bump them together (`rust-version = \"{floor}\"`)"
        ));
    }
    Ok(())
}

/// The quoted value of the first `key = "value"` line, ignoring comments.
///
/// A line scan rather than a TOML parser keeps this crate dependency-free; both
/// files hold the key once, on its own line.
fn toml_string(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        if name.trim() != key {
            return None;
        }
        let value = value.trim();
        let inner = value.strip_prefix('"')?;
        let end = inner.find('"')?;
        Some(inner[..end].to_owned())
    })
}

fn fmt() -> Result<(), String> {
    run("cargo", &["fmt", "--all", "--", "--check"])
}

fn lint() -> Result<(), String> {
    run(
        "cargo",
        &[
            "clippy",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ],
    )
}

/// Rustdoc must be warning-free.
///
/// A broken intra-doc link is invisible while writing and ships as a dead link on
/// docs.rs — the only documentation an embedder of these crates ever reads.
/// Nothing ran rustdoc before this step existed, which is precisely how nine of
/// them accumulated across six crates.
///
/// `--all-features` matches `lint` and `test`, so feature-gated items are
/// documented rather than skipped. It does **not** catch a link that resolves
/// only when a feature is on: these crates set no docs.rs `all-features`
/// metadata, so docs.rs builds them with default features, and a feature-gated
/// path must therefore not be an intra-doc link at all.
fn doc() -> Result<(), String> {
    run_with_env(
        "cargo",
        &["doc", "--workspace", "--no-deps", "--all-features"],
        &[("RUSTDOCFLAGS", "-D warnings")],
    )
}

fn test() -> Result<(), String> {
    run("cargo", &["test", "--workspace", "--all-features"])
}

/// The supply-chain gate. `cargo-deny` installs via cargo, so a missing binary is
/// a clear, actionable skip rather than a hard stop on a fresh checkout.
fn deny() -> Result<(), String> {
    if have("cargo", &["deny", "--version"]) {
        run("cargo", &["deny", "check"])
    } else {
        warn_skip("cargo-deny", "cargo install --locked cargo-deny");
        Ok(())
    }
}

/// Lint the helper shell scripts. `shellcheck` is a system tool, so a missing
/// binary is a skip; CI installs it so it always runs there.
fn shellcheck() -> Result<(), String> {
    if !have("shellcheck", &["--version"]) {
        warn_skip(
            "shellcheck",
            "your package manager (e.g. `apt install shellcheck`)",
        );
        return Ok(());
    }
    let scripts = shell_scripts()?;
    if scripts.is_empty() {
        return Ok(());
    }
    let refs: Vec<&str> = scripts.iter().map(String::as_str).collect();
    run("shellcheck", &refs)
}

/// Cross-build the fully static `fraisier` binary for musl.
///
/// fraisier's TLS stack pulls `aws-lc-sys`, which needs a C cross-compiler for
/// musl. `cargo-zigbuild` supplies one (zig) without a system musl toolchain,
/// which makes this reproducible on any machine and in CI.
fn dist(_args: &[String]) -> Result<(), String> {
    const TARGET: &str = "x86_64-unknown-linux-musl";
    if have("cargo", &["zigbuild", "--version"]) {
        run(
            "cargo",
            &[
                "zigbuild",
                "--release",
                "--target",
                TARGET,
                "--bin",
                "fraisier",
            ],
        )
    } else {
        Err(format!(
            "the static {TARGET} build needs cargo-zigbuild (zig as the C cross-compiler \
             for aws-lc-sys).\n  install: `cargo install --locked cargo-zigbuild` plus a zig \
             toolchain (https://ziglang.org/download/)\n  then:    `rustup target add {TARGET}`\n  \
             or build inside a container that already has a musl C toolchain."
        ))
    }
}

/// Run a command, streaming its output, and turn a non-zero exit into an error.
fn run(program: &str, args: &[&str]) -> Result<(), String> {
    run_with_env(program, args, &[])
}

/// Run `program`, echoing the command, with `envs` set for the child.
///
/// The echoed line includes the variables, so a failing gate step can be copied
/// out of the log and re-run verbatim.
fn run_with_env(program: &str, args: &[&str], envs: &[(&str, &str)]) -> Result<(), String> {
    let mut shown = String::new();
    for (key, value) in envs {
        let _ = write!(shown, "{key}=\"{value}\" ");
    }
    println!("\x1b[1;36m$ {shown}{program} {}\x1b[0m", args.join(" "));
    let mut command = Command::new(program);
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let status = command
        .status()
        .map_err(|error| format!("could not launch `{program}`: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{program} {}` failed ({status})", args.join(" ")))
    }
}

/// Whether a tool answers a version probe (used to detect optional gate tools).
fn have(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn warn_skip(tool: &str, install: &str) {
    eprintln!("\x1b[1;33m⚠ skipping {tool} (not installed) — install with: {install}\x1b[0m");
}

/// The `scripts/*.sh` files, sorted, relative to the workspace root.
fn shell_scripts() -> Result<Vec<String>, String> {
    let dir = std::path::Path::new("scripts");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut scripts = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| format!("reading scripts/: {error}"))? {
        let path = entry
            .map_err(|error| format!("reading scripts/: {error}"))?
            .path();
        if path.extension().is_some_and(|ext| ext == "sh") {
            if let Some(name) = path.to_str() {
                scripts.push(name.to_owned());
            }
        }
    }
    scripts.sort();
    Ok(scripts)
}

fn usage() {
    println!(
        "cargo xtask <task>\n\n\
         tasks:\n  \
         ci          the full gate: toolchain + fmt + clippy + doc + test + release build + deny + shellcheck\n  \
         toolchain   rust-toolchain.toml pins an exact X.Y.Z; rust-version is its X.Y\n  \
         fmt         cargo fmt --all -- --check\n  \
         lint        cargo clippy --all-targets --all-features -- -D warnings\n  \
         doc         RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --all-features\n  \
         test        cargo test --workspace --all-features\n  \
         deny        cargo deny check\n  \
         shellcheck  shellcheck scripts/*.sh\n  \
         dist        cross-build the static musl `fraisier` binary (cargo-zigbuild)"
    );
}

#[cfg(test)]
mod tests {
    use super::{check_toolchain, toml_string};

    #[test]
    fn a_floating_channel_is_refused() {
        assert!(check_toolchain("stable", "1.99").is_err());
    }

    #[test]
    fn a_minor_only_channel_is_refused() {
        assert!(check_toolchain("1.99", "1.99").is_err());
    }

    #[test]
    fn a_dated_nightly_is_refused() {
        assert!(check_toolchain("nightly-2026-09-25", "1.99").is_err());
    }

    #[test]
    fn an_exact_channel_matching_rust_version_is_accepted() {
        assert_eq!(check_toolchain("1.99.0", "1.99"), Ok(()));
    }

    #[test]
    fn a_rust_version_below_the_pin_is_refused() {
        assert!(check_toolchain("1.99.0", "1.95").is_err());
    }

    #[test]
    fn the_value_is_read_past_comments_and_spacing() {
        let text = "[toolchain]\n# channel = \"stable\"\nchannel   =  \"1.99.0\" # pinned\n";
        assert_eq!(toml_string(text, "channel").as_deref(), Some("1.99.0"));
    }

    #[test]
    fn a_similarly_named_key_does_not_match() {
        assert_eq!(
            toml_string("rust-version-x = \"1\"\n", "rust-version"),
            None
        );
    }
}

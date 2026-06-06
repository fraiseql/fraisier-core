//! Workspace task runner — the single source of truth for the CI gate.
//!
//! `cargo xtask ci` runs the exact checks CI runs, so "green locally" and "green
//! in CI" are the *same command* rather than two lists that drift apart. CI
//! invokes `cargo xtask ci` too, which makes the parity structural. Each check is
//! also runnable on its own (`cargo xtask fmt` / `lint` / `test` / `deny` /
//! `shellcheck`), and `cargo xtask dist` cross-builds the static musl binary.
//!
//! This crate deliberately has **zero dependencies** (std only): the task runner
//! that guards the supply chain must not add to it, and a dependency-free tool
//! never breaks because of upstream churn.

use std::process::{Command, ExitCode, Stdio};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((task, rest)) = args.split_first() else {
        usage();
        return ExitCode::FAILURE;
    };

    let result = match task.as_str() {
        "ci" => ci(),
        "fmt" => fmt(),
        "lint" => lint(),
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
    fmt()?;
    lint()?;
    test()?;
    run("cargo", &["build", "--release"])?;
    deny()?;
    shellcheck()?;
    println!("\n\x1b[1;32m✓ all gate checks passed\x1b[0m");
    Ok(())
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
    println!("\x1b[1;36m$ {program} {}\x1b[0m", args.join(" "));
    let status = Command::new(program)
        .args(args)
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
         ci          the full gate: fmt + clippy + test + release build + deny + shellcheck\n  \
         fmt         cargo fmt --all -- --check\n  \
         lint        cargo clippy --all-targets --all-features -- -D warnings\n  \
         test        cargo test --workspace --all-features\n  \
         deny        cargo deny check\n  \
         shellcheck  shellcheck scripts/*.sh\n  \
         dist        cross-build the static musl `fraisier` binary (cargo-zigbuild)"
    );
}

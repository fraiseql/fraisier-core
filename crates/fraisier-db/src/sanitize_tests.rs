use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use super::run;

use crate::PgConn;

fn conn() -> PgConn {
    PgConn::parse("postgres://app:s3cret@db.internal:5433/checkout_staging").expect("dsn")
}

fn me() -> u32 {
    std::fs::metadata("/proc/self").expect("proc").uid()
}

fn script(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

#[test]
fn an_executable_sees_only_path_and_the_app_pg_variables() {
    let dir = tempfile::tempdir().expect("tempdir");
    let seen = dir.path().join("env");
    let step = script(
        dir.path(),
        "sanitize.sh",
        &format!("#!/bin/sh\nenv > {}\n", seen.display()),
        0o700,
    );
    std::env::set_var("FRAISIER_SANITIZE_TEST_SECRET", "r00tpw");
    let ran = block_on(run(&step, &conn(), me())).expect("ran");
    std::env::remove_var("FRAISIER_SANITIZE_TEST_SECRET");
    assert_eq!(ran.kind, "executable");
    let env = std::fs::read_to_string(&seen).expect("env dumped");
    let names: Vec<&str> = env
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .collect();
    for name in &names {
        assert!(
            name.starts_with("PG") || ["PATH", "PWD", "SHLVL", "_"].contains(name),
            "{name} leaked into the sanitize step: {env}"
        );
    }
    assert!(env.contains("PGUSER=app"), "{env}");
    assert!(env.contains("PGDATABASE=checkout_staging"), "{env}");
    assert!(env.contains("PGPASSWORD=s3cret"), "{env}");
    assert!(!env.contains("r00tpw"), "{env}");
}

#[test]
fn the_step_gets_no_arguments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let seen = dir.path().join("args");
    let step = script(
        dir.path(),
        "sanitize.sh",
        &format!("#!/bin/sh\necho \"$#\" > {}\n", seen.display()),
        0o700,
    );
    block_on(run(&step, &conn(), me())).expect("ran");
    assert_eq!(std::fs::read_to_string(&seen).expect("args").trim(), "0");
}

#[test]
fn the_executable_is_hashed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let step = script(dir.path(), "sanitize.sh", "#!/bin/sh\nexit 0\n", 0o700);
    let ran = block_on(run(&step, &conn(), me())).expect("ran");
    assert_eq!(ran.sha256, super::sha256_hex(b"#!/bin/sh\nexit 0\n"));
    assert_eq!(ran.sha256.len(), 64);
}

#[test]
fn a_failing_executable_fails_the_step_with_its_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");
    let step = script(
        dir.path(),
        "sanitize.sh",
        "#!/bin/sh\necho 'could not scrub users' >&2\nexit 4\n",
        0o700,
    );
    let error = block_on(run(&step, &conn(), me())).expect_err("fails");
    assert!(error.contains("could not scrub users"), "{error}");
}

#[test]
fn a_group_or_world_writable_step_is_refused_before_it_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ran = dir.path().join("ran");
    for mode in [0o720, 0o702] {
        let step = script(
            dir.path(),
            "sanitize.sh",
            &format!("#!/bin/sh\ntouch {}\n", ran.display()),
            mode,
        );
        let error = block_on(run(&step, &conn(), me())).expect_err("refused");
        assert!(error.contains("writable"), "{error}");
        assert!(!ran.exists());
    }
}

#[test]
fn a_step_owned_by_someone_else_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let step = script(dir.path(), "sanitize.sh", "#!/bin/sh\nexit 0\n", 0o700);
    let error = block_on(run(&step, &conn(), me() + 1)).expect_err("refused");
    assert!(error.contains("owned by"), "{error}");
}

#[test]
fn a_sql_step_pipes_the_bytes_it_hashed_to_psql_in_one_transaction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).expect("mkdir");
    let seen = dir.path().join("psql");
    // A psql that records its argv and the stdin it was fed.
    script(
        &bin,
        "psql",
        &format!(
            "#!/bin/sh\necho \"$@\" > {0}.args\ncat > {0}.stdin\n",
            seen.display()
        ),
        0o700,
    );
    let step = script(
        dir.path(),
        "sanitize.sql",
        "UPDATE tb_user SET email = 'x';\n",
        0o600,
    );
    let ran = block_on(super::run_with(&bin.join("psql"), &step, &conn(), me())).expect("ran");
    assert_eq!(ran.kind, "sql");
    let args = std::fs::read_to_string(seen.with_extension("args")).expect("args");
    assert!(args.contains("ON_ERROR_STOP=1"), "{args}");
    assert!(args.split_whitespace().any(|a| a == "-1"), "{args}");
    assert!(
        !args.contains("s3cret") && !args.contains("db.internal"),
        "{args}"
    );
    assert_eq!(
        std::fs::read_to_string(seen.with_extension("stdin")).expect("stdin"),
        "UPDATE tb_user SET email = 'x';\n"
    );
    assert_eq!(
        ran.sha256,
        super::sha256_hex(b"UPDATE tb_user SET email = 'x';\n")
    );
}

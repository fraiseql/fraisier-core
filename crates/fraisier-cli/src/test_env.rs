//! Test-only serialization of the process environment.

/// Serializes every test that mutates **or observes** the whole process
/// environment.
///
/// `set_var`/`remove_var` are process-global, so a test that exports a variable
/// races any test that reads the environment — including one that spawns a
/// child process and asserts on what that child inherited. Both kinds have to
/// take the same lock or neither assertion is trustworthy: without it, a hook
/// test asserting *"fraisier exported exactly these four variables"* fails
/// whenever a db-op test happens to be mid-`set_var` on another thread.
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Drive a future to completion on a fresh current-thread runtime.
///
/// Tests holding [`ENV_LOCK`] stay synchronous so the guard is never held
/// across an `.await`.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

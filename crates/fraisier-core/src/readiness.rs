//! The database-readiness preflight check.
//!
//! A deployable can declare what its database server must offer (a PostgreSQL
//! major-version floor, pg_tviews). When it does, every deploy flow checks that
//! first, before anything else in preflight and before any mutation, and refuses
//! a server that is not ready. fraisier never installs what is missing; the
//! refusal says what is missing and how to fix it.
//!
//! The check fails closed: a probe that cannot run refuses the deploy rather
//! than letting it through unchecked.

use async_trait::async_trait;
use fraisier_saga::saga::SagaError;

use crate::adapter_axes::AdapterCtx;
use crate::restore_rehearsal::DATABASE_URL_SECRET;

/// Probe the live database for what the deployable declared it needs.
#[async_trait]
pub trait DatabaseReadiness: Send + Sync {
    /// Every reason the database at `live_dsn` is not ready, in a sentence each.
    /// Empty means ready.
    ///
    /// # Errors
    /// The probe could not run (the server is unreachable, the probe's output was
    /// not understood). The deploy is refused on this too.
    async fn unready(&self, live_dsn: &str) -> Result<Vec<String>, String>;
}

/// Run `probe` against the live database in `ctx`, as the `preflight` step.
///
/// # Errors
/// A [`SagaError::StepFailed`] on `preflight` naming every reason the database is
/// not ready, or why readiness could not be checked.
pub async fn check(probe: &dyn DatabaseReadiness, ctx: &AdapterCtx) -> Result<(), SagaError> {
    let refuse = |message: String| SagaError::StepFailed {
        step: "preflight".to_owned(),
        message,
    };
    let live_dsn = ctx.secret(DATABASE_URL_SECRET).map_err(|error| {
        refuse(format!(
            "could not check database readiness: the database DSN is unavailable: {error}"
        ))
    })?;
    let unready = probe
        .unready(&live_dsn)
        .await
        .map_err(|error| refuse(format!("could not check database readiness: {error}")))?;
    if unready.is_empty() {
        Ok(())
    } else {
        Err(refuse(format!(
            "the database is not ready for this deployable: {}",
            unready.join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use fraisier_saga::saga::SagaError;

    use super::{check, DatabaseReadiness};
    use crate::adapter_axes::AdapterCtx;

    struct Fake {
        answer: Result<Vec<String>, String>,
        asked: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(answer: Result<Vec<String>, String>) -> Self {
            Self {
                answer,
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl DatabaseReadiness for Fake {
        async fn unready(&self, live_dsn: &str) -> Result<Vec<String>, String> {
            self.asked.lock().expect("asked").push(live_dsn.to_owned());
            self.answer.clone()
        }
    }

    fn ctx() -> AdapterCtx {
        AdapterCtx::new("shop", "production")
            .with_resolved_secret("DATABASE_URL", "postgres://app:pw@db/shop".to_owned())
    }

    fn message(error: SagaError) -> (String, String) {
        match error {
            SagaError::StepFailed { step, message } => (step, message),
            other => panic!("expected a StepFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_ready_database_passes_and_is_asked_with_the_live_dsn() {
        let probe = Fake::new(Ok(Vec::new()));
        check(&probe, &ctx()).await.expect("ready");
        assert_eq!(
            *probe.asked.lock().expect("asked"),
            ["postgres://app:pw@db/shop"]
        );
    }

    #[tokio::test]
    async fn every_reason_is_named_in_the_preflight_failure() {
        let probe = Fake::new(Ok(vec![
            "the server runs PostgreSQL 17.5".to_owned(),
            "pg_tviews is not preloaded".to_owned(),
        ]));
        let (step, text) = message(check(&probe, &ctx()).await.expect_err("unready"));
        assert_eq!(step, "preflight");
        assert!(text.contains("not ready"), "{text}");
        assert!(text.contains("PostgreSQL 17.5"), "{text}");
        assert!(text.contains("pg_tviews is not preloaded"), "{text}");
    }

    #[tokio::test]
    async fn a_probe_that_cannot_run_refuses_the_deploy() {
        let probe = Fake::new(Err("connection refused".to_owned()));
        let (step, text) = message(check(&probe, &ctx()).await.expect_err("fail closed"));
        assert_eq!(step, "preflight");
        assert!(text.contains("could not check"), "{text}");
        assert!(text.contains("connection refused"), "{text}");
    }

    #[tokio::test]
    async fn no_dsn_refuses_the_deploy_without_asking() {
        let probe = Fake::new(Ok(Vec::new()));
        let (step, _) = message(
            check(&probe, &AdapterCtx::new("shop", "production"))
                .await
                .expect_err("no DSN"),
        );
        assert_eq!(step, "preflight");
        assert!(probe.asked.lock().expect("asked").is_empty());
    }
}

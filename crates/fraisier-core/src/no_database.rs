//! The migration axis of a deployable that owns no database (`adapter = "none"`).
//!
//! A router or a static front end has nothing to migrate. Declaring that
//! explicitly, rather than filling the axis with no-op commands, lets the saga
//! know there is nothing to compensate: a failed deploy rolls back to
//! `RolledBack`, not `PartialRollback` for want of a revision to return to.

use async_trait::async_trait;

use crate::adapter_axes::{
    AdapterCtx, AdapterDescription, AdapterError, MigrationAdapter, MigrationOutcome, Revision,
    VerifyReport,
};

/// The adapter name a config selects this axis with.
pub const ADAPTER_NAME: &str = "none";

/// A migration axis with no database behind it. Every method succeeds and
/// changes nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoDatabase;

#[async_trait]
impl MigrationAdapter for NoDatabase {
    async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
        Ok(AdapterDescription {
            name: ADAPTER_NAME.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: 1,
            capabilities: Vec::new(),
        })
    }

    async fn current_revision(&self, _ctx: &AdapterCtx) -> Result<Option<Revision>, AdapterError> {
        Ok(None)
    }

    async fn up(
        &self,
        _ctx: &AdapterCtx,
        _target: Option<Revision>,
    ) -> Result<MigrationOutcome, AdapterError> {
        Ok(MigrationOutcome::default())
    }

    async fn down_to(
        &self,
        _ctx: &AdapterCtx,
        _target: Revision,
    ) -> Result<MigrationOutcome, AdapterError> {
        Ok(MigrationOutcome::default())
    }

    async fn verify(&self, _ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
        Ok(VerifyReport::new(true).with_skipped(true))
    }

    fn owns_database(&self) -> bool {
        false
    }
}

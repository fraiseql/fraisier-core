//! # fraisier-saga
//!
//! A generic, atomic **saga state machine** with rollback semantics, a pluggable
//! [`StateStore`] persistence trait, and OpenTelemetry-native event emission.
//!
//! This crate is the **engine** layer of fraisier (PRD §5.1, Layer 1). It is
//! deliberately *not* deploy-specific: it models any multi-step operation whose
//! steps each have a forward action and a compensating action, persists progress
//! through a [`StateStore`], and rolls back in reverse on failure. The deploy
//! composition lives one layer up in `fraisier-core`.
//!
//! ## Stability
//!
//! Phase 1 is foundational and the public surface is **not frozen**. The
//! [`StateStore`] trait and the [`SagaEvent`] / [`SagaState`] types are designed
//! deliberately (the trait against the hardest backend — Postgres with N writers —
//! per PRD risk row), but the saga *driver* API is a working skeleton and will
//! change before the day-3 trait freeze.
//!
//! [`StateStore`]: crate::state_store::StateStore
//! [`SagaEvent`]: crate::events::SagaEvent
//! [`SagaState`]: crate::events::SagaState

pub mod events;

#[cfg(feature = "otel")]
pub mod otel;

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
//! The [`StateStore`] trait and the [`SagaEvent`] / [`SagaState`] types are
//! designed against the hardest backend (Postgres with N writers). The saga
//! *driver* API ([`Saga`], [`Step`], etc.) is the stable v1.0 contract; types
//! expected to grow are `#[non_exhaustive]`.
//!
//! [`Saga`]: crate::saga::Saga
//! [`Step`]: crate::saga::Step
//!
//! [`StateStore`]: crate::state_store::StateStore
//! [`SagaEvent`]: crate::events::SagaEvent
//! [`SagaState`]: crate::events::SagaState

pub mod events;
pub mod saga;
pub mod state_store;

#[cfg(feature = "otel")]
pub mod otel;

/// Compile the embedding guide's runnable example as a doctest so it cannot rot.
///
/// The guide (`docs/guides/embedding-fraisier-saga.md`) shows a third-party app
/// authoring its own compensating steps over the public engine API. Including it
/// under `#[cfg(doctest)]` runs its example via `cargo test --doc` without adding
/// it to the rendered crate docs.
#[cfg(doctest)]
#[doc = include_str!("../../../docs/guides/embedding-fraisier-saga.md")]
pub struct EmbeddingGuide;

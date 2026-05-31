//! # fraisier-core
//!
//! The **deploy layer** of fraisier (PRD §5.1, Layer 2). It composes the generic
//! [`fraisier-saga`](https://docs.rs/fraisier-saga) engine into deploy-specific
//! flows and defines the five **adapter axis traits** every deploy is built from.
//!
//! ## Adapter axes
//!
//! [`adapter_axes`] defines the contract for each axis (PRD §6.1):
//!
//! - [`ArtifactAdapter`](adapter_axes::ArtifactAdapter) — get code/binary onto a host
//! - [`MigrationAdapter`](adapter_axes::MigrationAdapter) — run database migrations
//! - [`ServiceAdapter`](adapter_axes::ServiceAdapter) — start/stop/restart a service
//! - [`HealthAdapter`](adapter_axes::HealthAdapter) — verify a host is serving
//! - [`LbAdapter`](adapter_axes::LbAdapter) — drain/reattach a host at the load balancer
//!
//! ## The convergence rule
//!
//! Every adapter trait argument and return type is `Serialize + Deserialize`. That
//! single constraint is what lets the **in-process** adapters and the **IPC**
//! (JSON-RPC over stdio) adapters implement the *same* trait: an IPC adapter is
//! just a transport that serializes each call. A method that needs a
//! non-serializable type is a wrong method, not a missing capability.
//!
//! ## Crate-graph rule
//!
//! `fraisier-core` depends on `fraisier-saga` only and must never depend on
//! `fraisier-ipc`; concrete adapter wiring happens at the `fraisier-cli` /
//! embedder layer (see the crate README).

pub mod adapter_axes;
pub mod multi_host;

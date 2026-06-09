//! Typed saga events and the generic saga lifecycle state.
//!
//! # OpenTelemetry attribute conventions
//!
//! Every state-transition span is named `saga.state_transition` and carries the
//! shared `deploy.*` attribute namespace:
//!
//! | Attribute            | Meaning                                |
//! |----------------------|----------------------------------------|
//! | `deploy.fraise`      | the fraise (deployable) name           |
//! | `deploy.environment` | the target environment                 |
//! | `deploy.from_state`  | the lifecycle state being left         |
//! | `deploy.to_state`    | the lifecycle state being entered      |
//!
//! The deploy layer (`fraisier-core`) reuses the same namespace for its own
//! adapter-call spans, so a deploy reads as one coherent trace.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The generic lifecycle state of a saga run.
///
/// The engine is deliberately *not* deploy-specific: intermediate forward and
/// compensating steps are named by string, so the deploy layer can label them
/// (`"preflight"`, `"migrate"`, …) without the engine knowing those names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SagaState {
    /// No run in progress — the starting state and the fully-rolled-back state.
    Idle,
    /// The forward step named by the payload is executing or has completed.
    Running(String),
    /// All steps completed and the run is committed.
    Committed,
    /// The compensating action for the named step is executing.
    Compensating(String),
    /// All completed steps were compensated; back at the pre-run baseline.
    RolledBack,
    /// The run failed outright; the payload is a human-readable reason.
    Failed(String),
    /// Rollback itself partially failed; operator intervention is required.
    ///
    /// The payload explains which compensation did not complete. The engine
    /// never pretends a failed rollback succeeded (PRD §5.4).
    PartialRollback(String),
}

impl fmt::Display for SagaState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => f.write_str("idle"),
            Self::Running(step) => write!(f, "running:{step}"),
            Self::Committed => f.write_str("committed"),
            Self::Compensating(step) => write!(f, "compensating:{step}"),
            Self::RolledBack => f.write_str("rolled_back"),
            Self::Failed(_) => f.write_str("failed"),
            Self::PartialRollback(_) => f.write_str("partial_rollback"),
        }
    }
}

/// A typed event emitted at a significant point in a saga run.
///
/// Events are both persisted (via the state store) and surfaced as OTel spans,
/// so an operator's tracing backend reconstructs "what happened" (PRD §5.6).
/// The set of event kinds is intentionally small; it may grow (adapter calls,
/// rollback markers) in future versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SagaEvent {
    /// The saga moved from one lifecycle state to another.
    StateTransition {
        /// The state being left.
        from: SagaState,
        /// The state being entered.
        to: SagaState,
    },
}

/// Open the root span for one saga run, named `saga.deploy`.
///
/// Hold it (via [`tracing::Instrument`]) for the whole run so every
/// state-transition span nests beneath it and the run exports as a **single
/// trace per deploy** rather than one disconnected trace per transition. When the
/// `otel` feature is enabled and a pipeline is installed via [`crate::otel`], the
/// span is exported; otherwise it is a cheap no-op unless another subscriber is set.
#[must_use]
pub fn instrument_deploy_run(fraise: &str, environment: &str) -> tracing::Span {
    tracing::info_span!(
        "saga.deploy",
        deploy.fraise = fraise,
        deploy.environment = environment,
    )
}

/// Open a span recording a saga state transition.
///
/// Uses the `deploy.*` OTel attribute convention (see the module docs) and nests
/// under the current [`instrument_deploy_run`] span, so it appears within that
/// deploy's trace. When the `otel` feature is enabled and a pipeline is installed
/// via [`crate::otel`], the span is exported; otherwise it is a cheap no-op unless
/// another subscriber is set.
#[must_use]
pub fn instrument_state_transition(
    fraise: &str,
    environment: &str,
    from: &SagaState,
    to: &SagaState,
) -> tracing::Span {
    tracing::info_span!(
        "saga.state_transition",
        deploy.fraise = fraise,
        deploy.environment = environment,
        deploy.from_state = %from,
        deploy.to_state = %to,
    )
}

#[cfg(test)]
mod tests {
    use super::{instrument_state_transition, SagaEvent, SagaState};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::Attributes;
    use tracing::Id;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
    use tracing_subscriber::registry::LookupSpan;
    use tracing_subscriber::Registry;

    #[test]
    fn state_transition_round_trips_through_serde() {
        let event = SagaEvent::StateTransition {
            from: SagaState::Idle,
            to: SagaState::Running("preflight".to_owned()),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let back: SagaEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, back);
    }

    #[derive(Default)]
    struct FieldCapture(HashMap<String, String>);

    impl Visit for FieldCapture {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    /// `(span name, captured fields)` pairs, shared between the layer and the test.
    type CapturedSpans = Arc<Mutex<Vec<(String, HashMap<String, String>)>>>;

    #[derive(Clone, Default)]
    struct CapturingLayer {
        spans: CapturedSpans,
    }

    impl<S> Layer<S> for CapturingLayer
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
            let mut cap = FieldCapture::default();
            attrs.record(&mut cap);
            self.spans
                .lock()
                .expect("lock")
                .push((attrs.metadata().name().to_owned(), cap.0));
        }
    }

    #[test]
    fn state_transition_emits_span_with_deploy_attributes() {
        let layer = CapturingLayer::default();
        let captured = layer.spans.clone();
        let subscriber = Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            let _span = instrument_state_transition(
                "fraiseql",
                "production",
                &SagaState::Idle,
                &SagaState::Running("preflight".to_owned()),
            );
        });

        let spans = captured.lock().expect("lock").clone();
        assert_eq!(spans.len(), 1, "exactly one transition span emitted");
        let (name, fields) = &spans[0];
        assert_eq!(name, "saga.state_transition");
        assert_eq!(
            fields.get("deploy.fraise").map(String::as_str),
            Some("fraiseql")
        );
        assert_eq!(
            fields.get("deploy.environment").map(String::as_str),
            Some("production")
        );
        assert_eq!(
            fields.get("deploy.from_state").map(String::as_str),
            Some("idle")
        );
        assert_eq!(
            fields.get("deploy.to_state").map(String::as_str),
            Some("running:preflight")
        );
    }
}

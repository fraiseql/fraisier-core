# fraisier-saga

Generic, atomic **saga state machine** with rollback semantics, a pluggable
`StateStore` persistence trait, and OpenTelemetry-native event emission.

This is the engine layer of [fraisier](https://github.com/fraiseql/fraisier)
(PRD §5.1, Layer 1). It is deliberately *not* deploy-specific: it models any
multi-step operation whose steps each have a forward action and a compensating
action, persisting progress through a `StateStore` and rolling back in reverse
on failure.

> **Stability:** the saga driver API (`Saga`, `Step`, `StepContext`, `SagaError`,
> `SagaOutcome`) is the frozen v1.0 contract; types expected to grow are
> `#[non_exhaustive]`.

## License

Licensed under either of MIT or Apache-2.0 at your option.

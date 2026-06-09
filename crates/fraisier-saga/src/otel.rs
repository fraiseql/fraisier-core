//! OpenTelemetry / OTLP export wiring (compiled only under the `otel` feature).
//!
//! This module turns the spans emitted by [`crate::events`] into exported OTLP
//! traces. It is feature-gated so library embedders that do not want the
//! OpenTelemetry dependency tree (PRD risk row: "OTel adds dependency weight to
//! specql-platform") pay nothing; the CLI enables `otel` and calls [`install`].
//!
//! Transport is OTLP over HTTP/protobuf (the default exporter features), which
//! avoids pulling the gRPC/tonic stack into the build.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig as _};
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// `service.name` reported on the resource of every exported span.
const SERVICE_NAME: &str = "fraisier";
/// Instrumentation-scope name for the engine's tracer.
const SCOPE: &str = "fraisier-saga";

/// Errors that can occur while installing the OTLP export pipeline.
#[derive(Debug, thiserror::Error)]
pub enum OtelError {
    /// The OTLP span exporter could not be constructed.
    #[error("failed to build the OTLP span exporter: {0}")]
    Exporter(#[from] opentelemetry_otlp::ExporterBuildError),
    /// A global `tracing` subscriber was already installed.
    #[error("failed to install the global tracing subscriber: {0}")]
    Install(#[from] tracing_subscriber::util::TryInitError),
}

/// Flushes and shuts down the tracer provider when dropped.
///
/// Keep it alive for as long as you want spans exported — typically for the
/// whole process. Dropping it tears the pipeline down and flushes pending spans.
#[must_use = "dropping the guard immediately tears the export pipeline down"]
pub struct OtelGuard {
    provider: SdkTracerProvider,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Err(error) = self.provider.shutdown() {
            tracing::warn!(%error, "OTLP tracer provider shutdown failed");
        }
    }
}

/// Install a global `tracing` subscriber that exports spans over OTLP/HTTP and
/// return a guard that flushes on drop.
///
/// Pass `endpoint = None` to use the OTLP default (`http://localhost:4318`).
///
/// # Errors
///
/// Returns [`OtelError::Exporter`] if the exporter cannot be built (for example
/// an invalid endpoint), or [`OtelError::Install`] if a global subscriber has
/// already been installed in this process.
pub fn install(endpoint: Option<&str>) -> Result<OtelGuard, OtelError> {
    let mut builder = SpanExporter::builder().with_http();
    if let Some(endpoint) = endpoint {
        builder = builder.with_endpoint(endpoint);
    }
    let exporter = builder.build()?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(Resource::builder().with_service_name(SERVICE_NAME).build())
        .build();

    let tracer = provider.tracer(SCOPE);
    tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .try_init()?;

    Ok(OtelGuard { provider })
}

//! Logging, tracing and metrics setup.
//!
//! Every log line goes to stderr, as text or JSON, and carries the fields of
//! the spans it was emitted in: the `run` root span (with its `run_id`), the
//! phase (`sync_customers` / `sync_documents`), the item (`document` /
//! `customer`) and the HTTP request.
//!
//! The spans are additionally exported as OpenTelemetry traces over OTLP
//! (HTTP/protobuf) when `OTEL_EXPORTER_OTLP_ENDPOINT` or
//! `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` is set. The other standard `OTEL_*`
//! variables (headers, timeout, service name, resource attributes) apply as
//! usual. One run is one trace, and the `run_id` in the logs is its trace id.
//!
//! Metrics (the sync status snapshot, see [`crate::sync::report`]) are exported
//! over OTLP the same way when `OTEL_EXPORTER_OTLP_ENDPOINT` or
//! `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` is set, through the global meter
//! provider. They are sent once, on [`Telemetry::shutdown`].

use std::time::Duration;

use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use snafu::{ResultExt, Snafu};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{fmt, EnvFilter, Layer as _};

use crate::cli::LogFormat;

/// Service name reported to the trace backend unless `OTEL_SERVICE_NAME` is set.
const SERVICE_NAME: &str = "easybill-xero";

/// Timeout for one OTLP export request.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for buffered spans and metrics to be exported on exit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// Errors setting up the OTLP exporters.
#[derive(Debug, Snafu)]
pub enum Error {
    /// The HTTP client for the OTLP exporter could not be built.
    #[snafu(display("Failed to build the OTLP HTTP client"))]
    HttpClient { source: reqwest::Error },

    /// The OTLP span exporter could not be built.
    #[snafu(display("Failed to build the OTLP span exporter"))]
    SpanExporter {
        source: opentelemetry_otlp::ExporterBuildError,
    },

    /// The OTLP metric exporter could not be built.
    #[snafu(display("Failed to build the OTLP metric exporter"))]
    MetricExporter {
        source: opentelemetry_otlp::ExporterBuildError,
    },
}

/// Keeps the OTLP exporters alive; [`Telemetry::shutdown`] flushes them.
#[must_use = "dropping the guard without `shutdown` loses buffered spans and metrics"]
pub struct Telemetry {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl Telemetry {
    /// Exports the metrics, flushes buffered spans and stops the exporters. A
    /// no-op without OTLP.
    ///
    /// Runs on a blocking thread: the exporters wait for their requests, which
    /// themselves run on this runtime.
    pub async fn shutdown(self) {
        let Self {
            tracer_provider,
            meter_provider,
        } = self;
        let result = tokio::task::spawn_blocking(move || {
            let mut errors = Vec::new();
            if let Some(provider) = meter_provider {
                if let Err(e) = provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT) {
                    errors.push(format!("exporting metrics failed: {e}"));
                }
            }
            if let Some(provider) = tracer_provider {
                if let Err(e) = provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT) {
                    errors.push(format!("exporting traces failed: {e}"));
                }
            }
            errors
        })
        .await;
        // The subscriber is still installed, but the run span is closed;
        // stderr is the only place left to report this.
        match result {
            Ok(errors) => {
                for error in errors {
                    eprintln!("Warning: {error}");
                }
            }
            Err(e) => eprintln!("Warning: exporting telemetry failed: {e}"),
        }
    }
}

/// Initialises `tracing` to stderr in the given format, honouring `RUST_LOG`
/// and defaulting to `info`, plus OTLP trace and metric export if configured
/// (see the module docs). Text output is coloured only on a terminal.
///
/// User-facing results are written to stdout with `println!`; diagnostics go
/// through `tracing` on stderr so the two streams can be redirected separately.
///
/// Must be called inside the Tokio runtime: the exporter sends on it.
///
/// # Errors
///
/// Returns the errors of OTLP exporters that are configured but can't be set
/// up. Logging is initialised either way.
pub fn init(format: LogFormat) -> (Telemetry, Vec<Error>) {
    use std::io::IsTerminal as _;

    let mut errors = Vec::new();
    let tracer_provider = configured("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .then(tracer_provider)
        .and_then(|result| result.map_err(|e| errors.push(e)).ok());
    let meter_provider = configured("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
        .then(meter_provider)
        .and_then(|result| result.map_err(|e| errors.push(e)).ok());
    if let Some(provider) = &meter_provider {
        opentelemetry::global::set_meter_provider(provider.clone());
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = fmt::layer().with_writer(std::io::stderr);
    let fmt_layer = match format {
        LogFormat::Text => fmt_layer.with_ansi(std::io::stderr().is_terminal()).boxed(),
        // `span` is the innermost span, `spans` all of them from the root.
        LogFormat::Json => fmt_layer
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .boxed(),
    };
    let otel_layer = tracer_provider
        .as_ref()
        .map(|provider| tracing_opentelemetry::layer().with_tracer(provider.tracer(SERVICE_NAME)));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .init();

    (
        Telemetry {
            tracer_provider,
            meter_provider,
        },
        errors,
    )
}

/// The id of the run that `span` (the root span) belongs to: its trace id
/// when traces are exported, so logs and traces share one id, else a random
/// id of the same shape.
pub fn run_id(span: &tracing::Span) -> String {
    let context = span.context();
    let span_ref = context.span();
    let span_context = span_ref.span_context();
    if span_context.is_valid() {
        return span_context.trace_id().to_string();
    }
    format!("{:032x}", rand::random::<u128>())
}

/// Whether an OTLP endpoint is configured for a signal: its own variable
/// (`signal_endpoint`) or the shared `OTEL_EXPORTER_OTLP_ENDPOINT`.
fn configured(signal_endpoint: &str) -> bool {
    ["OTEL_EXPORTER_OTLP_ENDPOINT", signal_endpoint]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
}

/// Builds the tracer provider exporting spans in batches over OTLP/HTTP.
fn tracer_provider() -> Result<SdkTracerProvider, Error> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(OtlpClient::new()?)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .context(SpanExporterSnafu)?;
    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource())
        .build())
}

/// Builds the meter provider exporting metrics over OTLP/HTTP.
///
/// The periodic reader also exports on shutdown, which is the export that
/// counts: the status snapshot is only recorded at the end of a run.
fn meter_provider() -> Result<SdkMeterProvider, Error> {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_http_client(OtlpClient::new()?)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .context(MetricExporterSnafu)?;
    Ok(SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter).build())
        .with_resource(resource())
        .build())
}

/// The OTLP resource: the service name (unless `OTEL_SERVICE_NAME` is set)
/// and version, plus `OTEL_RESOURCE_ATTRIBUTES`.
///
/// Nothing unique per process (such as `service.instance.id`) is added: the
/// metric series of one run must replace those of the previous one.
fn resource() -> Resource {
    let mut resource = Resource::builder();
    if std::env::var_os("OTEL_SERVICE_NAME").is_none() {
        resource = resource.with_service_name(SERVICE_NAME);
    }
    resource
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            env!("CARGO_PKG_VERSION"),
        ))
        .build()
}

/// OTLP HTTP client on the app's own `reqwest` (and its rustls setup).
///
/// The batch span processor exports from its own thread, outside the Tokio
/// runtime, so each request is spawned onto the runtime and awaited there.
#[derive(Debug)]
struct OtlpClient {
    http: reqwest::Client,
    runtime: tokio::runtime::Handle,
}

impl OtlpClient {
    /// A client sending on the current Tokio runtime.
    fn new() -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .timeout(EXPORT_TIMEOUT)
            .build()
            .context(HttpClientSnafu)?;
        Ok(Self {
            http,
            runtime: tokio::runtime::Handle::current(),
        })
    }
}

#[async_trait::async_trait]
impl opentelemetry_http::HttpClient for OtlpClient {
    async fn send_bytes(
        &self,
        request: http::Request<bytes::Bytes>,
    ) -> Result<http::Response<bytes::Bytes>, opentelemetry_http::HttpError> {
        let http = self.http.clone();
        let task = self.runtime.spawn(async move {
            let request = reqwest::Request::try_from(request)?;
            let response = http.execute(request).await?;
            let mut builder = http::Response::builder().status(response.status());
            if let Some(headers) = builder.headers_mut() {
                *headers = response.headers().clone();
            }
            let body = response.bytes().await?;
            Ok::<_, opentelemetry_http::HttpError>(builder.body(body)?)
        });
        task.await?
    }
}

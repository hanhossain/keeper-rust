use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use opentelemetry::context::FutureExt;
use opentelemetry::trace::{TraceContextExt, Tracer};
use opentelemetry::{Context, global};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use serde::Serialize;
use service_common::middleware::metrics::RequestMetricsLayer;
use service_common::middleware::trace::RequestTraceLayer;
use service_common::shutdown_signal;
use std::sync::LazyLock;
use std::time::Duration;
use tower::ServiceBuilder;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

static RESOURCE: LazyLock<Resource> = LazyLock::new(|| {
    Resource::builder()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .build()
});

fn init_tracer() -> anyhow::Result<SdkTracerProvider> {
    let exporter = SpanExporter::builder().with_tonic().build()?;

    let provider = SdkTracerProvider::builder()
        .with_resource(RESOURCE.clone())
        .with_batch_exporter(exporter)
        .build();
    global::set_tracer_provider(provider.clone());
    Ok(provider)
}

fn init_metrics() -> anyhow::Result<SdkMeterProvider> {
    let exporter = MetricExporter::builder().with_tonic().build()?;

    let provider = SdkMeterProvider::builder()
        .with_resource(RESOURCE.clone())
        .with_periodic_exporter(exporter)
        .build();

    global::set_meter_provider(provider.clone());
    Ok(provider)
}

fn init_logs() -> anyhow::Result<SdkLoggerProvider> {
    let exporter = LogExporter::builder().with_tonic().build()?;

    let provider = SdkLoggerProvider::builder()
        .with_resource(RESOURCE.clone())
        .with_batch_exporter(exporter)
        .build();

    let otel_layer = OpenTelemetryTracingBridge::new(&provider).with_filter(
        EnvFilter::new("info")
            .add_directive("backend_service=trace".parse()?)
            .add_directive("service_common=trace".parse()?)
            .add_directive("hyper=off".parse()?)
            .add_directive("tonic=off".parse()?)
            .add_directive("h2=off".parse()?)
            .add_directive("reqwest=off".parse()?)
            .add_directive("tower=off".parse()?)
            .add_directive("tower_http=trace".parse()?)
            .add_directive("axum=trace".parse()?),
    );

    tracing_subscriber::registry()
        .with(otel_layer)
        .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::new("info")))
        .init();

    Ok(provider)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());

    let logger_provider = init_logs()?;
    let tracer_provider = init_tracer()?;
    let meter_provider = init_metrics()?;

    let middleware = ServiceBuilder::new()
        .layer(RequestTraceLayer::new())
        .layer(RequestMetricsLayer::new());

    let app = Router::new()
        .route("/random", get(random_handler))
        .layer(middleware);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3001").await?;

    tracing::debug!("listening on {}", listener.local_addr()?);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracer_provider.shutdown()?;
    meter_provider.shutdown()?;
    logger_provider.shutdown()?;

    Ok(())
}

#[derive(Serialize)]
struct RandResponse {
    seconds: f64,
}

async fn random_handler() -> (StatusCode, Json<RandResponse>) {
    let fut = async {
        let rand_val = rand::random();
        tracing::info!(
            name = "random-delay",
            delay = rand_val,
            "delaying with random"
        );
        tokio::time::sleep(Duration::from_secs_f64(rand_val)).await;
        tracing::info!(name = "random-done", "done delaying with random");
        rand_val
    };

    let span = global::tracer("backend-service").start("random_handler");
    let cx = Context::current_with_span(span);
    let rand_val = fut.with_context(cx).await;

    (StatusCode::OK, Json(RandResponse { seconds: rand_val }))
}

use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use opentelemetry::context::FutureExt;
use opentelemetry::global;
use opentelemetry::trace::Tracer;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use serde::Serialize;
use service_common::middleware::metrics::RequestMetricsLayer;
use service_common::middleware::trace::RequestTraceLayer;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::signal;
use tower::ServiceBuilder;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

static RESOURCE: LazyLock<Resource> = LazyLock::new(|| {
    Resource::builder()
        .with_service_name("backend-service")
        .build()
});

fn init_tracer() -> SdkTracerProvider {
    let exporter = SpanExporter::builder().with_tonic().build().unwrap();

    let provider = SdkTracerProvider::builder()
        .with_resource(RESOURCE.clone())
        .with_batch_exporter(exporter)
        .build();
    global::set_tracer_provider(provider.clone());
    provider
}

fn init_metrics() -> SdkMeterProvider {
    let exporter = MetricExporter::builder().with_tonic().build().unwrap();

    let provider = SdkMeterProvider::builder()
        .with_resource(RESOURCE.clone())
        .with_periodic_exporter(exporter)
        .build();

    global::set_meter_provider(provider.clone());
    provider
}

fn init_logs() -> SdkLoggerProvider {
    let exporter = LogExporter::builder().with_tonic().build().unwrap();

    let provider = SdkLoggerProvider::builder()
        .with_resource(RESOURCE.clone())
        .with_batch_exporter(exporter)
        .build();

    let otel_layer = OpenTelemetryTracingBridge::new(&provider).with_filter(
        EnvFilter::new("info")
            .add_directive("backend_service=trace".parse().unwrap())
            .add_directive("hyper=off".parse().unwrap())
            .add_directive("tonic=off".parse().unwrap())
            .add_directive("h2=off".parse().unwrap())
            .add_directive("reqwest=off".parse().unwrap())
            .add_directive("tower=off".parse().unwrap())
            .add_directive("tower_http=trace".parse().unwrap())
            .add_directive("axum=trace".parse().unwrap()),
    );

    tracing_subscriber::registry()
        .with(otel_layer)
        .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::new("info")))
        .init();

    provider
}

#[tokio::main]
async fn main() {
    let logger_provider = init_logs();
    let tracer_provider = init_tracer();
    let meter_provider = init_metrics();

    let middleware = ServiceBuilder::new()
        .layer(RequestTraceLayer::new())
        .layer(RequestMetricsLayer::new());

    let app = Router::new()
        .route("/random", get(random_handler))
        .layer(middleware);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3001")
        .await
        .unwrap();

    tracing::debug!("listening on {}", listener.local_addr().unwrap());

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    let _ = tracer_provider.shutdown();
    let _ = meter_provider.shutdown();
    let _ = logger_provider.shutdown();
}

#[derive(Serialize)]
struct RandResponse {
    seconds: f64,
}

async fn random_handler() -> (StatusCode, Json<RandResponse>) {
    let _span = global::tracer("backend-service").start("random_handler");
    let rand_val = rand::random();
    // TODO: these logs are getting linked to the root span instead of the child span
    tracing::info!(
        name = "random-delay",
        delay = rand_val,
        "delaying with random"
    );
    tokio::time::sleep(Duration::from_secs_f64(rand_val))
        .with_current_context()
        .await;
    tracing::info!(name = "random-done", "done delaying with random");
    (StatusCode::OK, Json(RandResponse { seconds: rand_val }))
}

async fn shutdown_signal() {
    let ctrl_c = async { signal::ctrl_c().await.unwrap() };
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .unwrap()
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {}
    }
}

use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use opentelemetry::trace::{TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{KeyValue, global};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use serde::{Deserialize, Serialize};
use service_common::error::AppError;
use service_common::middleware::metrics::RequestMetricsLayer;
use service_common::middleware::trace::RequestTraceLayer;
use std::sync::LazyLock;
use std::time::Duration;
use tower::ServiceBuilder;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

static RESOURCE: LazyLock<Resource> =
    LazyLock::new(|| Resource::builder().with_service_name("api-service").build());

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
            .add_directive("api_service=trace".parse().unwrap())
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

    do_stuff(&tracer_provider);

    let middleware = ServiceBuilder::new()
        .layer(RequestTraceLayer::new())
        .layer(RequestMetricsLayer::new())
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::new(Duration::from_secs(10)));
    let app = Router::new().route("/ping", get(ping)).layer(middleware);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();

    tracing::debug!("listening on {}", listener.local_addr().unwrap());

    axum::serve(listener, app)
        .with_graceful_shutdown(service_common::shutdown_signal())
        .await
        .unwrap();

    let _ = tracer_provider.shutdown();
    let _ = meter_provider.shutdown();
    let _ = logger_provider.shutdown();
}

fn do_stuff(tracer_provider: &SdkTracerProvider) {
    let tracer = tracer_provider.tracer("my-tracer");
    tracer.in_span("Main operation", |cx| {
        let span = cx.span();
        span.add_event("Nice operation!".to_string(), vec![KeyValue::new("bogons", 100)]);
        span.set_attribute(KeyValue::new("another.key", "yes"));

        tracing::info!(name: "my-event-inside-span", target: "my-target", "hello from {}. My price is {}. I am also inside a Span!", "banana", 2.99);

        tracer.in_span("Sub operation...", |cx| {
            let span = cx.span();
            span.set_attribute(KeyValue::new("another.key", "yes"));
            span.add_event("Sub span event", vec![]);
        });
    });

    tracing::info!(name: "my-event", target: "my-target", "hello from {}. My price is {}", "apple", 1.99);
}

async fn ping() -> Result<(StatusCode, Json<Ping>), AppError> {
    let res = reqwest::get("http://localhost:3001/random")
        .await?
        .json::<BackendResponse>()
        .await?;
    Ok((
        StatusCode::OK,
        Json(Ping {
            ping: "Pong".to_string(),
            delay_seconds: res.seconds,
        }),
    ))
}

#[derive(Serialize)]
struct Ping {
    ping: String,
    delay_seconds: f64,
}

#[derive(Deserialize)]
struct BackendResponse {
    seconds: f64,
}

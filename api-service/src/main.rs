use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use opentelemetry::global;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_tracing::OtelPathNames;
use serde::{Deserialize, Serialize};
use service_common::error::AppError;
use service_common::middleware::client_trace::ReqwestTracingMiddleware;
use service_common::middleware::metrics::RequestMetricsLayer;
use service_common::middleware::trace::RequestTraceLayer;
use std::sync::LazyLock;
use std::time::Duration;
use tower::ServiceBuilder;
use tower_http::timeout::TimeoutLayer;
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
            .add_directive("api_service=trace".parse()?)
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

#[derive(Clone)]
struct AppState {
    backend_client: ClientWithMiddleware,
}

impl AppState {
    fn new() -> anyhow::Result<AppState> {
        let client = reqwest::Client::builder().build()?;
        let backend_client = ClientBuilder::new(client)
            .with(ReqwestTracingMiddleware)
            .build();
        Ok(AppState { backend_client })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());

    let logger_provider = init_logs()?;
    let tracer_provider = init_tracer()?;
    let meter_provider = init_metrics()?;

    let middleware = ServiceBuilder::new()
        .layer(RequestTraceLayer::new())
        .layer(RequestMetricsLayer::new())
        .layer(TimeoutLayer::new(Duration::from_secs(10)));

    let app_state = AppState::new()?;
    let app = Router::new()
        .route("/ping", get(ping))
        .layer(middleware)
        .with_state(app_state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;

    tracing::debug!("listening on {}", listener.local_addr()?);

    axum::serve(listener, app)
        .with_graceful_shutdown(service_common::shutdown_signal())
        .await?;

    tracer_provider.shutdown()?;
    meter_provider.shutdown()?;
    logger_provider.shutdown()?;

    Ok(())
}

async fn ping(State(state): State<AppState>) -> Result<(StatusCode, Json<Ping>), AppError> {
    let res = state
        .backend_client
        .get("http://localhost:3001/random")
        .with_extension(OtelPathNames::known_paths(["/random"])?)
        .send()
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

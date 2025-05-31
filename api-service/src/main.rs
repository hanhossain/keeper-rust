use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::LogExporter;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use serde::Serialize;
use std::sync::LazyLock;
use tower_http::trace::TraceLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

static RESOURCE: LazyLock<Resource> =
    LazyLock::new(|| Resource::builder().with_service_name("api-service").build());

fn init_logs() -> SdkLoggerProvider {
    let exporter = LogExporter::builder().with_tonic().build().unwrap();

    let provider = SdkLoggerProvider::builder()
        .with_resource(RESOURCE.clone())
        .with_batch_exporter(exporter)
        .build();

    let otel_layer = OpenTelemetryTracingBridge::new(&provider).with_filter(
        EnvFilter::new("info")
            .add_directive("api-service=trace".parse().unwrap())
            .add_directive("hyper=off".parse().unwrap())
            .add_directive("tonic=off".parse().unwrap())
            .add_directive("h2=off".parse().unwrap())
            .add_directive("reqwest=off".parse().unwrap())
            .add_directive("tower=off".parse().unwrap())
            .add_directive("tower_http=debug".parse().unwrap())
            .add_directive("axum=trace".parse().unwrap()),
    );

    tracing_subscriber::registry()
        .with(otel_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_thread_names(true)
                .with_filter(EnvFilter::new("info")),
        )
        .init();

    provider
}

#[tokio::main]
async fn main() {
    let _logger_provider = init_logs();

    let app = Router::new()
        .route("/ping", get(ping))
        .layer(TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn ping() -> (StatusCode, Json<Ping>) {
    (
        StatusCode::OK,
        Json(Ping {
            ping: "Pong".to_string(),
        }),
    )
}

#[derive(Serialize)]
struct Ping {
    ping: String,
}

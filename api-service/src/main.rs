use axum::extract::{MatchedPath, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use opentelemetry::metrics::MeterProvider;
use opentelemetry::trace::{Span, SpanKind, Status, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{KeyValue, global};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_semantic_conventions::attribute::URL_QUERY;
use opentelemetry_semantic_conventions::trace::{
    HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, HTTP_ROUTE, NETWORK_PROTOCOL_VERSION, URL_PATH,
    URL_SCHEME,
};
use serde::Serialize;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::signal;
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
            .add_directive("api-service=trace".parse().unwrap())
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

    let meter = meter_provider.meter("some-meter");
    let counter = meter
        .u64_counter("test_counter")
        .with_description("display purposes")
        .build();

    for _ in 0..10 {
        counter.add(1, &[KeyValue::new("test_key", "test_value")])
    }

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

    let middleware = ServiceBuilder::new()
        .layer(axum::middleware::from_fn(telemetry_middleware))
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::new(Duration::from_secs(10)));
    let app = Router::new().route("/ping", get(ping)).layer(middleware);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
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

async fn telemetry_middleware(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();

    let mut attributes = vec![
        KeyValue::new(HTTP_REQUEST_METHOD, method.to_string()),
        KeyValue::new(URL_PATH, uri.path().to_string()),
        KeyValue::new(URL_SCHEME, uri.scheme_str().unwrap_or("http").to_string()),
        // TODO: consider trimming HTTP/ from the version
        KeyValue::new(NETWORK_PROTOCOL_VERSION, format!("{:?}", request.version())),
    ];

    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str());

    if let Some(route) = route {
        attributes.push(KeyValue::new(HTTP_ROUTE, route.to_owned()));
    }

    if let Some(query) = uri.query() {
        attributes.push(KeyValue::new(URL_QUERY, query.to_owned()));
    }

    let tracer = global::tracer("api-service");
    let mut span = tracer
        .span_builder(route.map_or(method.to_string(), |route| format!("{method} {route}")))
        .with_kind(SpanKind::Server)
        .with_attributes(attributes)
        .start(&tracer);

    // TODO: set required and recommended server span attributes
    // https://opentelemetry.io/docs/specs/semconv/http/http-spans/#http-server-span
    // error.type

    // TODO: do I need to set the context?
    // let cx = Context::current_with_span(span);
    // let _guard = cx.attach();
    let response = next.run(request).await;
    let status_code = response.status();

    span.set_attribute(KeyValue::new(
        HTTP_RESPONSE_STATUS_CODE,
        status_code.as_u16() as i64,
    ));

    if status_code.is_server_error() {
        span.set_status(Status::error(""));
    }

    response
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

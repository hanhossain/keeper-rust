use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use opentelemetry::trace::{FutureExt, SpanKind, TraceContextExt, Tracer};
use opentelemetry::{Context, global};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use opentelemetry_semantic_conventions::metric::{
    HTTP_CLIENT_REQUEST_DURATION, HTTP_SERVER_ACTIVE_REQUESTS, HTTP_SERVER_REQUEST_DURATION,
};
use pretty_assertions::assert_eq;
use reqwest_middleware::ClientBuilder;
use reqwest_middleware::reqwest::Client;
use reqwest_tracing::OtelPathNames;
use service_common::middleware::client_metrics::ReqwestMetricsMiddleware;
use service_common::middleware::client_trace::ReqwestTracingMiddleware;
use service_common::middleware::metrics::RequestMetricsLayer;
use service_common::middleware::trace::RequestTraceLayer;
use std::collections::HashSet;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower::ServiceBuilder;

fn setup_traces() -> (InMemorySpanExporter, SdkTracerProvider) {
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    global::set_tracer_provider(tracer_provider.clone());
    global::set_text_map_propagator(TraceContextPropagator::new());
    (span_exporter, tracer_provider)
}

fn setup_metrics() -> (InMemoryMetricExporter, SdkMeterProvider) {
    let metric_exporter = InMemoryMetricExporter::default();
    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter.clone())
        .build();
    global::set_meter_provider(meter_provider.clone());
    (metric_exporter, meter_provider)
}

async fn spawn_server() -> SocketAddr {
    let app = Router::new()
        .route(
            "/foo",
            get(|| async {
                let tracer = global::tracer("server-tracer");
                tracer.in_span("server-child", |cx| {
                    cx.span().add_event("hello", Vec::new());
                });
            }),
        )
        .layer(
            ServiceBuilder::new()
                .layer(RequestTraceLayer::new())
                .layer(RequestMetricsLayer::new()),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    addr
}

async fn run_client(addr: &SocketAddr) {
    let client = ClientBuilder::new(Client::new())
        .with(ReqwestTracingMiddleware::new())
        .with(ReqwestMetricsMiddleware::new())
        .build();

    let root_span = global::tracer("client-tracer").start("client-root");
    let cx = Context::current_with_span(root_span);

    let response = client
        .get(format!("http://{}/foo", addr))
        .with_extension(OtelPathNames::known_paths(["/foo"]).unwrap())
        .send()
        .with_context(cx.clone())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn combine_everything() {
    let (span_exporter, tracer_provider) = setup_traces();
    let (metric_exporter, meter_provider) = setup_metrics();

    let addr = spawn_server().await;

    run_client(&addr).await;

    tracer_provider.force_flush().unwrap();
    meter_provider.force_flush().unwrap();

    let spans = span_exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 4);

    let client_root = spans
        .iter()
        .find(|s| s.instrumentation_scope.name() == "client-tracer")
        .unwrap();
    assert_eq!(client_root.name, "client-root");
    assert_eq!(client_root.span_kind, SpanKind::Internal);

    let client_child = spans
        .iter()
        .find(|s| {
            s.instrumentation_scope.name() == "service-common" && s.span_kind == SpanKind::Client
        })
        .unwrap();
    assert_eq!(client_child.name, "GET /foo");

    let server_root = spans
        .iter()
        .find(|s| {
            s.instrumentation_scope.name() == "service-common" && s.span_kind == SpanKind::Server
        })
        .unwrap();
    assert_eq!(server_root.name, "GET /foo");

    let server_child = spans
        .iter()
        .find(|s| s.instrumentation_scope.name() == "server-tracer")
        .unwrap();
    assert_eq!(server_child.name, "server-child");
    assert_eq!(server_child.span_kind, SpanKind::Internal);

    let resource_metrics = metric_exporter.get_finished_metrics().unwrap();
    let metric_names: HashSet<_> = resource_metrics[0]
        .scope_metrics()
        .next()
        .unwrap()
        .metrics()
        .map(|m| m.name())
        .collect();
    assert_eq!(
        metric_names,
        HashSet::from([
            HTTP_SERVER_REQUEST_DURATION,
            HTTP_SERVER_ACTIVE_REQUESTS,
            HTTP_CLIENT_REQUEST_DURATION
        ])
    );
}

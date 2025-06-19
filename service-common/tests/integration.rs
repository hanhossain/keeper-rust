use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::routing::get;
use opentelemetry::global;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use opentelemetry_semantic_conventions::metric::{
    HTTP_SERVER_ACTIVE_REQUESTS, HTTP_SERVER_REQUEST_DURATION,
};
use pretty_assertions::assert_eq;
use service_common::middleware::metrics::RequestMetricsLayer;
use service_common::middleware::trace::RequestTraceLayer;
use std::collections::HashSet;
use tower::ServiceExt;

#[tokio::test]
async fn combine_everything() {
    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    global::set_tracer_provider(tracer_provider.clone());

    let metric_exporter = InMemoryMetricExporter::default();
    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter.clone())
        .build();
    global::set_meter_provider(meter_provider.clone());

    let app = Router::new()
        .route("/", get(|| async {}))
        .layer(RequestMetricsLayer::new())
        .layer(RequestTraceLayer::new());

    let _ = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    tracer_provider.force_flush().unwrap();
    meter_provider.force_flush().unwrap();

    let spans = span_exporter.get_finished_spans().unwrap();
    assert_eq!(spans[0].name, "GET /");

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
        HashSet::from([HTTP_SERVER_REQUEST_DURATION, HTTP_SERVER_ACTIVE_REQUESTS])
    );
}

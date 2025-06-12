use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::trace::{Span, SpanKind, Status, Tracer};
use opentelemetry::{KeyValue, global};
use opentelemetry_semantic_conventions::attribute::{
    HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, HTTP_ROUTE, NETWORK_PROTOCOL_VERSION, URL_PATH,
    URL_QUERY, URL_SCHEME,
};

const PKG_NAME: &str = env!("CARGO_PKG_NAME");

pub async fn telemetry_middleware(request: Request, next: Next) -> Response {
    let method = request.method();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str());
    let scheme = request.uri().scheme_str().unwrap_or("http");

    let tracer = global::tracer(PKG_NAME);

    let span_name = route.map_or(method.to_string(), |route| format!("{method} {route}"));

    let mut attributes = vec![
        KeyValue::new(HTTP_REQUEST_METHOD, method.to_string()),
        KeyValue::new(URL_SCHEME, scheme.to_string()),
        // TODO: consider trimming HTTP/ from the version
        KeyValue::new(NETWORK_PROTOCOL_VERSION, format!("{:?}", request.version())),
    ];

    if let Some(route) = route {
        attributes.push(KeyValue::new(HTTP_ROUTE, route.to_owned()));
    }

    attributes.push(KeyValue::new(URL_PATH, request.uri().path().to_string()));

    if let Some(query) = request.uri().query() {
        attributes.push(KeyValue::new(URL_QUERY, query.to_owned()));
    }

    let mut span = tracer
        .span_builder(span_name)
        .with_kind(SpanKind::Server)
        .with_attributes(attributes)
        .start(&tracer);

    let response = next.run(request).await;
    let status_code = response.status();

    let status_code_attribute =
        KeyValue::new(HTTP_RESPONSE_STATUS_CODE, status_code.as_u16() as i64);
    span.set_attribute(status_code_attribute.clone());

    if status_code.is_server_error() {
        span.set_status(Status::error(""));
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use opentelemetry::InstrumentationScope;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use std::sync::OnceLock;
    use tower::ServiceExt;

    static TELEMETRY_CONTEXT: OnceLock<TelemetryContext> = OnceLock::new();

    #[derive(Clone)]
    struct TelemetryContext {
        span_exporter: InMemorySpanExporter,
    }

    impl TelemetryContext {
        fn new() -> TelemetryContext {
            TELEMETRY_CONTEXT
                .get_or_init(|| {
                    let span_exporter = InMemorySpanExporter::default();
                    let tracer_provider = SdkTracerProvider::builder()
                        .with_simple_exporter(span_exporter.clone())
                        .build();
                    global::set_tracer_provider(tracer_provider);

                    TelemetryContext { span_exporter }
                })
                .clone()
        }
    }

    #[tokio::test]
    async fn traces() {
        let telemetry_context = TelemetryContext::new();
        let app = Router::new()
            .route("/ping/{id}", get(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn(telemetry_middleware));

        let _ = app
            .oneshot(
                Request::builder()
                    .uri("/ping/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let spans = telemetry_context
            .span_exporter
            .get_finished_spans()
            .unwrap();
        let span = &spans[0];

        assert_eq!(span.name, "GET /ping/{id}");
        assert_eq!(span.span_kind, SpanKind::Server);
        assert_eq!(
            span.instrumentation_scope,
            InstrumentationScope::builder(PKG_NAME).build()
        );

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(URL_SCHEME, "http"),
            KeyValue::new(NETWORK_PROTOCOL_VERSION, "HTTP/1.1"),
            KeyValue::new(HTTP_ROUTE, "/ping/{id}"),
            KeyValue::new(URL_PATH, "/ping/1"),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 200),
        ];
        assert_eq!(span.attributes, attributes);
    }
}

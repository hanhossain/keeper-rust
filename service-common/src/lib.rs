use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::global;
use opentelemetry::trace::Tracer;

pub async fn telemetry_middleware(request: Request, next: Next) -> Response {
    let method = request.method();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str());

    let tracer = global::tracer("");

    let span_name = route.map_or(method.to_string(), |route| format!("{method} {route}"));
    let _span = tracer.span_builder(span_name).start(&tracer);

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use crate::telemetry_middleware;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use opentelemetry::global;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use tower::ServiceExt;

    #[tokio::test]
    async fn span_path_found() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        global::set_tracer_provider(provider.clone());

        let app = Router::new()
            .route("/ping", get(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn(telemetry_middleware));

        let _ = app
            .oneshot(Request::builder().uri("/ping").body(Body::empty()).unwrap())
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();

        assert_eq!(spans[0].name, "GET /ping");
    }
}

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::global;
use opentelemetry::trace::Tracer;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");

pub async fn telemetry_middleware(request: Request, next: Next) -> Response {
    let method = request.method();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str());

    let tracer = global::tracer(PKG_NAME);

    let span_name = route.map_or(method.to_string(), |route| format!("{method} {route}"));
    let _span = tracer.span_builder(span_name).start(&tracer);

    next.run(request).await
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
    use tower::ServiceExt;

    #[tokio::test]
    async fn traces() {
        let exporter = InMemorySpanExporter::default();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        global::set_tracer_provider(tracer_provider.clone());

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

        tracer_provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();

        assert_eq!(spans[0].name, "GET /ping/{id}");
        assert_eq!(
            spans[0].instrumentation_scope,
            InstrumentationScope::builder(PKG_NAME).build()
        );
        dbg!(spans);
    }
}

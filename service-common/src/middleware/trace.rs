use crate::PKG_NAME;
use axum::extract::{MatchedPath, Request};
use axum::response::Response;
use futures_util::future::BoxFuture;
use opentelemetry::KeyValue;
use opentelemetry::trace::{Span, SpanKind, Tracer, TracerProvider};
use opentelemetry_semantic_conventions::trace::HTTP_RESPONSE_STATUS_CODE;
use std::task::{Context, Poll};
use tower::{Layer, Service};

#[derive(Clone)]
pub struct RequestTraceLayer<T> {
    tracer: T,
}

impl<T> RequestTraceLayer<T> {
    pub fn new<P>(tracer_provider: P) -> RequestTraceLayer<T>
    where
        T: Tracer,
        P: TracerProvider<Tracer = T>,
    {
        let tracer = tracer_provider.tracer(PKG_NAME);
        RequestTraceLayer { tracer }
    }
}

impl<S, T> Layer<S> for RequestTraceLayer<T>
where
    T: Clone,
{
    type Service = RequestTraceMiddleware<S, T>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestTraceMiddleware {
            inner,
            tracer: self.tracer.clone(),
        }
    }
}

#[derive(Clone)]
pub struct RequestTraceMiddleware<S, T> {
    inner: S,
    tracer: T,
}

impl<S, T, Sp> Service<Request> for RequestTraceMiddleware<S, T>
where
    S: Service<Request, Response = Response> + Send + 'static,
    S::Future: Send + 'static,
    T: Tracer<Span = Sp>,
    Sp: Span + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let method = request.method();
        let route = request
            .extensions()
            .get::<MatchedPath>()
            .map(|p| p.as_str());
        let span_name = route.map_or(method.to_string(), |route| format!("{method} {route}"));

        let mut span = self
            .tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Server)
            .start(&self.tracer);

        let f = self.inner.call(request);
        Box::pin(async move {
            let response = f.await?;

            span.set_attribute(KeyValue::new(
                HTTP_RESPONSE_STATUS_CODE,
                response.status().as_u16() as i64,
            ));

            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use opentelemetry::SpanId;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use tower::ServiceExt;

    #[tokio::test]
    async fn root_span_single_request() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        let app = Router::new()
            .route("/", get(|| async {}))
            .layer(RequestTraceLayer::new(provider.clone()));

        let _ = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();

        assert_eq!(spans.len(), 1);

        let span = &spans[0];
        assert_eq!(span.name, "GET /");
        assert_eq!(span.parent_span_id, SpanId::from_u64(0));
        assert_eq!(span.span_kind, SpanKind::Server);
        assert_eq!(span.instrumentation_scope.name(), PKG_NAME);

        assert_eq!(
            span.attributes,
            vec![KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 200)]
        );
    }

    #[tokio::test]
    async fn root_spans_multiple_requests() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        let mut app = Router::new()
            .route("/foo", get(|| async {}))
            .route("/bar", get(|| async {}))
            .layer(RequestTraceLayer::new(provider.clone()));

        let _ = ServiceExt::<Request<Body>>::ready(&mut app)
            .await
            .unwrap()
            .call(Request::builder().uri("/foo").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let _ = ServiceExt::<Request<Body>>::ready(&mut app)
            .await
            .unwrap()
            .call(Request::builder().uri("/bar").body(Body::empty()).unwrap())
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        dbg!(&spans);

        assert_eq!(spans.len(), 2);

        let span = &spans[0];
        assert_eq!(span.name, "GET /foo");
        assert_eq!(span.parent_span_id, SpanId::from_u64(0));

        let span = &spans[1];
        assert_eq!(span.name, "GET /bar");
        assert_eq!(span.parent_span_id, SpanId::from_u64(0));
    }
}

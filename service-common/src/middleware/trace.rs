use crate::PKG_NAME;
use axum::extract::{MatchedPath, Request};
use axum::response::Response;
use futures_util::future::BoxFuture;
use opentelemetry::KeyValue;
use opentelemetry::trace::{Span, SpanKind, Status, Tracer, TracerProvider};
use opentelemetry_semantic_conventions::attribute::{
    HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, HTTP_ROUTE, NETWORK_PROTOCOL_VERSION, URL_PATH,
    URL_QUERY, URL_SCHEME,
};
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
        let span_name =
            route.map_or_else(|| method.to_string(), |route| format!("{method} {route}"));
        let scheme = request.uri().scheme_str().unwrap_or("http");

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

        let mut span = self
            .tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Server)
            .with_attributes(attributes)
            .start(&self.tracer);

        let future = self.inner.call(request);
        Box::pin(async move {
            let response = future.await?;

            span.set_attribute(KeyValue::new(
                HTTP_RESPONSE_STATUS_CODE,
                response.status().as_u16() as i64,
            ));

            if response.status().is_server_error() {
                span.set_status(Status::error(""));
            }

            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
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
        assert_eq!(span.status, Status::Unset);

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(URL_SCHEME, "http"),
            KeyValue::new(NETWORK_PROTOCOL_VERSION, "HTTP/1.1"),
            KeyValue::new(HTTP_ROUTE, "/"),
            KeyValue::new(URL_PATH, "/"),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 200),
        ];
        assert_eq!(span.attributes, attributes);
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

        assert_eq!(spans.len(), 2);

        let span = &spans[0];
        assert_eq!(span.name, "GET /foo");
        assert_eq!(span.parent_span_id, SpanId::from_u64(0));

        let span = &spans[1];
        assert_eq!(span.name, "GET /bar");
        assert_eq!(span.parent_span_id, SpanId::from_u64(0));
    }

    #[tokio::test]
    async fn root_span_parameterized_path() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        let app = Router::new()
            .route("/foo/{id}", get(|| async {}))
            .layer(RequestTraceLayer::new(provider.clone()));

        let _ = app
            .oneshot(
                Request::builder()
                    .uri("/foo/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();

        assert_eq!(spans.len(), 1);

        let span = &spans[0];
        assert_eq!(span.name, "GET /foo/{id}");
        assert_eq!(span.parent_span_id, SpanId::from_u64(0));
        assert_eq!(span.span_kind, SpanKind::Server);
        assert_eq!(span.instrumentation_scope.name(), PKG_NAME);

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(URL_SCHEME, "http"),
            KeyValue::new(NETWORK_PROTOCOL_VERSION, "HTTP/1.1"),
            KeyValue::new(HTTP_ROUTE, "/foo/{id}"),
            KeyValue::new(URL_PATH, "/foo/1"),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 200),
        ];
        assert_eq!(span.attributes, attributes);
    }

    #[tokio::test]
    async fn root_span_url_query() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        let app = Router::new()
            .route("/foo", get(|| async {}))
            .layer(RequestTraceLayer::new(provider.clone()));

        let _ = app
            .oneshot(
                Request::builder()
                    .uri("/foo?query=value")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();

        assert_eq!(spans.len(), 1);

        let span = &spans[0];
        assert_eq!(span.name, "GET /foo");
        assert_eq!(span.parent_span_id, SpanId::from_u64(0));
        assert_eq!(span.span_kind, SpanKind::Server);
        assert_eq!(span.instrumentation_scope.name(), PKG_NAME);

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(URL_SCHEME, "http"),
            KeyValue::new(NETWORK_PROTOCOL_VERSION, "HTTP/1.1"),
            KeyValue::new(HTTP_ROUTE, "/foo"),
            KeyValue::new(URL_PATH, "/foo"),
            KeyValue::new(URL_QUERY, "query=value"),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 200),
        ];
        assert_eq!(span.attributes, attributes);
    }

    #[tokio::test]
    async fn root_span_server_error() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        let app = Router::new()
            .route("/", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
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
        assert_eq!(span.status, Status::error(""));

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(URL_SCHEME, "http"),
            KeyValue::new(NETWORK_PROTOCOL_VERSION, "HTTP/1.1"),
            KeyValue::new(HTTP_ROUTE, "/"),
            KeyValue::new(URL_PATH, "/"),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 500),
        ];
        assert_eq!(span.attributes, attributes);
    }
}

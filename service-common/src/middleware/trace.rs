use crate::{PKG_NAME, SpanExt};
use axum::extract::{MatchedPath, Request};
use axum::response::Response;
use futures_util::future::BoxFuture;
use opentelemetry::context::FutureExt;
use opentelemetry::global::GlobalTracerProvider;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{KeyValue, global};
use opentelemetry_http::HeaderExtractor;
use opentelemetry_semantic_conventions::attribute::{
    HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, HTTP_ROUTE, NETWORK_PROTOCOL_VERSION, URL_PATH,
    URL_QUERY, URL_SCHEME,
};
use opentelemetry_semantic_conventions::trace::ERROR_TYPE;
use std::error::Error;
use std::task;
use std::task::Poll;
use tower::{Layer, Service};

#[derive(Clone)]
pub struct RequestTraceLayer<P> {
    tracer_provider: P,
}

impl RequestTraceLayer<GlobalTracerProvider> {
    pub fn new() -> Self {
        Self::new_with_provider(global::tracer_provider())
    }
}

impl<P> RequestTraceLayer<P> {
    pub fn new_with_provider(tracer_provider: P) -> RequestTraceLayer<P> {
        RequestTraceLayer { tracer_provider }
    }
}

impl<S, P: Clone> Layer<S> for RequestTraceLayer<P> {
    type Service = RequestTraceMiddleware<S, P>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestTraceMiddleware {
            inner,
            tracer_provider: self.tracer_provider.clone(),
        }
    }
}

#[derive(Clone)]
pub struct RequestTraceMiddleware<S, P> {
    inner: S,
    tracer_provider: P,
}

impl<S, P, B> Service<Request> for RequestTraceMiddleware<S, P>
where
    S: Service<Request, Response = Response<B>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Error,
    P: TracerProvider,
    <<P as TracerProvider>::Tracer as Tracer>::Span: Send + Sync + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let parent_context = global::get_text_map_propagator(|propagator| {
            propagator.extract(&HeaderExtractor(request.headers()))
        });
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

        let tracer = self.tracer_provider.tracer(PKG_NAME);

        let span = tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Server)
            .with_attributes(attributes)
            .start_with_context(&tracer, &parent_context);

        let cx = parent_context.with_span(span);
        let _guard = cx.clone().attach();
        tracing::debug!("request starting");

        let future = self.inner.call(request).with_current_context();
        Box::pin(async move {
            let response = future.await;
            let _guard = cx.clone().attach();
            let span = cx.span();

            let result = match response {
                Ok(res) => {
                    span.set_attribute(KeyValue::new(
                        HTTP_RESPONSE_STATUS_CODE,
                        res.status().as_u16() as i64,
                    ));

                    if res.status().is_server_error() {
                        tracing::error!("request failed with handled error");
                        span.set_status(Status::error(""));
                        span.set_attribute(KeyValue::new(
                            ERROR_TYPE,
                            res.status().as_str().to_string(),
                        ));
                    }

                    Ok(res)
                }
                Err(error) => {
                    // TODO: add exception.stacktrace
                    tracing::error!("request failed with unhandled error");
                    span.record_error(&error);
                    span.set_status(Status::error(""));
                    span.set_attribute(KeyValue::new(ERROR_TYPE, error.to_string()));
                    span.record_error_ext(&error as &dyn Error);
                    Err(error)
                }
            };

            tracing::debug!("request ending");
            span.end();
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use opentelemetry::trace::{Span, get_active_span};
    use opentelemetry::{SpanId, TraceId};
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use opentelemetry_semantic_conventions::trace::{EXCEPTION_MESSAGE, EXCEPTION_STACKTRACE};
    use pretty_assertions::{assert_eq, assert_ne};
    use tower::{ServiceBuilder, ServiceExt};

    #[tokio::test]
    async fn root_span_single_request() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        let app = Router::new()
            .route("/", get(|| async {}))
            .layer(RequestTraceLayer::new_with_provider(provider.clone()));

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
    async fn root_span_single_request_global_tracer() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        global::set_tracer_provider(provider.clone());

        let app = Router::new()
            .route("/", get(|| async {}))
            .layer(RequestTraceLayer::new());

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
            .layer(RequestTraceLayer::new_with_provider(provider.clone()));

        let _ = ServiceExt::<Request>::ready(&mut app)
            .await
            .unwrap()
            .call(Request::builder().uri("/foo").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let _ = ServiceExt::<Request>::ready(&mut app)
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
            .layer(RequestTraceLayer::new_with_provider(provider.clone()));

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
            .layer(RequestTraceLayer::new_with_provider(provider.clone()));

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
    async fn root_span_status_500() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        let app = Router::new()
            .route("/", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            .layer(RequestTraceLayer::new_with_provider(provider.clone()));

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
            KeyValue::new(ERROR_TYPE, "500"),
        ];
        assert_eq!(span.attributes, attributes);
    }

    #[tokio::test]
    async fn root_span_service_error() {
        #[derive(Debug)]
        struct TestError;
        impl std::fmt::Display for TestError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("TestError")
            }
        }
        impl Error for TestError {}

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        let svc = ServiceBuilder::new()
            .layer(RequestTraceLayer::new_with_provider(provider.clone()))
            .service_fn(|_: Request<Body>| async { Err::<_, TestError>(TestError) });

        let response: Result<Response<Body>, _> = svc.oneshot(Request::new(Body::empty())).await;
        assert!(response.is_err());

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);

        assert_eq!(spans[0].name, "GET");
        assert_eq!(spans[0].status, Status::error(""));
        assert_eq!(spans[0].events.events[0].name, "exception");
        assert_eq!(
            spans[0].events.events[0].attributes,
            vec![KeyValue::new("exception.message", "TestError")]
        );

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(URL_SCHEME, "http"),
            KeyValue::new(NETWORK_PROTOCOL_VERSION, "HTTP/1.1"),
            KeyValue::new(URL_PATH, "/"),
            KeyValue::new(ERROR_TYPE, "TestError"),
        ];
        assert_eq!(spans[0].attributes, attributes);
    }

    #[tokio::test]
    async fn root_span_from_async_thread() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    get_active_span(|span| {
                        span.add_event("hello", Vec::new());
                    })
                }),
            )
            .layer(RequestTraceLayer::new_with_provider(provider.clone()));

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
        assert_eq!(span.events.events[0].name, "hello");
    }

    #[tokio::test]
    async fn child_spans() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let provider2 = provider.clone();

        let app = Router::new()
            .route(
                "/",
                get(|| async move {
                    let tracer = provider2.tracer("test");
                    tracer.in_span("child span 1", |cx| {
                        let span = cx.span();
                        span.add_event("from child span 1", Vec::new());
                    });

                    let mut span = tracer.start("child span 2");
                    span.add_event("from child span 2", Vec::new());
                }),
            )
            .layer(RequestTraceLayer::new_with_provider(provider.clone()));

        let _ = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();

        assert_eq!(spans.len(), 3);

        let child1 = &spans[0];
        let child2 = &spans[1];
        let parent = &spans[2];

        // verify parent span
        assert_eq!(parent.name, "GET /");
        assert_eq!(parent.parent_span_id, SpanId::from_u64(0));
        assert_eq!(parent.instrumentation_scope.name(), PKG_NAME);

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(URL_SCHEME, "http"),
            KeyValue::new(NETWORK_PROTOCOL_VERSION, "HTTP/1.1"),
            KeyValue::new(HTTP_ROUTE, "/"),
            KeyValue::new(URL_PATH, "/"),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 200),
        ];
        assert_eq!(parent.attributes, attributes);

        // verify child span 1
        assert_eq!(child1.name, "child span 1");
        assert_eq!(child1.parent_span_id, parent.span_context.span_id());
        assert_eq!(child1.instrumentation_scope.name(), "test");
        assert_eq!(
            child1.span_context.trace_id(),
            parent.span_context.trace_id()
        );
        assert_eq!(child1.events.events[0].name, "from child span 1");

        // verify child span 2
        assert_eq!(child2.name, "child span 2");
        assert_eq!(child2.parent_span_id, parent.span_context.span_id());
        assert_eq!(child2.instrumentation_scope.name(), "test");
        assert_eq!(
            child2.span_context.trace_id(),
            parent.span_context.trace_id()
        );
        assert_eq!(child2.events.events[0].name, "from child span 2");
        assert_ne!(child2.span_context.span_id(), child1.span_context.span_id());
    }

    #[tokio::test]
    async fn root_span_anyhow_error() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        async fn handler() -> Result<(), AppError> {
            try_thing()?;
            Ok(())
        }

        fn try_thing() -> Result<(), anyhow::Error> {
            anyhow::bail!("it failed!");
        }

        let app = Router::new()
            .route("/", get(handler))
            .layer(RequestTraceLayer::new_with_provider(provider.clone()));

        let _ = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();

        assert_eq!(spans.len(), 1);

        let span = &spans[0];
        assert_eq!(span.name, "GET /");
        assert_eq!(span.status, Status::error(""));
        assert_eq!(span.events.events[0].name, "exception");
        assert_eq!(
            span.events.events[0].attributes[0],
            KeyValue::new(EXCEPTION_MESSAGE, "it failed!")
        );

        assert_eq!(
            span.events.events[0].attributes[1].key.as_str(),
            EXCEPTION_STACKTRACE
        );

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(URL_SCHEME, "http"),
            KeyValue::new(NETWORK_PROTOCOL_VERSION, "HTTP/1.1"),
            KeyValue::new(HTTP_ROUTE, "/"),
            KeyValue::new(URL_PATH, "/"),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 500),
            KeyValue::new(ERROR_TYPE, "500"),
        ];
        assert_eq!(span.attributes, attributes);
    }

    #[tokio::test]
    async fn child_span_remote_request() {
        global::set_text_map_propagator(TraceContextPropagator::new());

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();

        let app = Router::new()
            .route("/", get(|| async {}))
            .layer(RequestTraceLayer::new_with_provider(provider.clone()));

        let _ = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(
                        "traceparent",
                        "00-00000000000000000000000000000001-0000000000000002-01",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();

        assert_eq!(spans.len(), 1);

        let span = &spans[0];
        assert_eq!(span.name, "GET /");
        assert_eq!(span.span_context.trace_id(), TraceId::from_u128(1));
        assert_eq!(span.parent_span_id, SpanId::from_u64(2));
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
}

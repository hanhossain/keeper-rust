use crate::{PKG_NAME, SpanExt};
use axum::http::Extensions;
use opentelemetry::context::FutureExt;
use opentelemetry::global::GlobalTracerProvider;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{Context, KeyValue, global};
use opentelemetry_http::HeaderInjector;
use opentelemetry_semantic_conventions::trace::{
    ERROR_TYPE, HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, SERVER_ADDRESS, SERVER_PORT,
    URL_FULL,
};
use reqwest_middleware::reqwest::{Request, Response};
use reqwest_middleware::{Middleware, Next};
use reqwest_tracing::default_span_name;
use std::error::Error;

// TODO: add tests
pub struct ReqwestTracingMiddleware<P> {
    tracer_provider: P,
}

impl ReqwestTracingMiddleware<GlobalTracerProvider> {
    // TODO: add to integration test
    pub fn new() -> Self {
        Self::new_with_provider(global::tracer_provider())
    }
}

impl<P> ReqwestTracingMiddleware<P> {
    pub fn new_with_provider(tracer_provider: P) -> ReqwestTracingMiddleware<P> {
        ReqwestTracingMiddleware { tracer_provider }
    }
}

#[async_trait::async_trait]
impl<P> Middleware for ReqwestTracingMiddleware<P>
where
    P: TracerProvider + Send + Sync + 'static,
    <P as TracerProvider>::Tracer: Send,
    <<P as TracerProvider>::Tracer as Tracer>::Span: Send + Sync,
{
    async fn handle(
        &self,
        mut req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        let tracer = self.tracer_provider.tracer(PKG_NAME);
        let span_name = default_span_name(&req, extensions).to_string();

        let method = req.method().to_string();
        let host = req.url().host_str().unwrap().to_string();
        let port = req.url().port();

        let mut attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, method),
            KeyValue::new(SERVER_ADDRESS, host),
            KeyValue::new(URL_FULL, req.url().as_str().to_string()),
        ];

        if let Some(port) = port {
            attributes.push(KeyValue::new(SERVER_PORT, port as i64));
        }

        let span = tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Client)
            .with_attributes(attributes)
            .start(&tracer);
        let cx = Context::current_with_span(span);

        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&cx, &mut HeaderInjector(req.headers_mut()))
        });

        let res = next.run(req, extensions).with_context(cx.clone()).await;

        let _guard = cx.clone().attach();
        let span = cx.span();
        match &res {
            Ok(response) => {
                let status = response.status();

                span.set_attribute(KeyValue::new(
                    HTTP_RESPONSE_STATUS_CODE,
                    status.as_u16() as i64,
                ));

                if status.is_client_error() || status.is_server_error() {
                    span.set_status(Status::error(""));
                    span.set_attribute(KeyValue::new(ERROR_TYPE, status.as_str().to_string()));
                    tracing::error!(status_code = ?status, "client received error status");
                }
            }
            Err(error) => {
                // TODO: test
                span.set_status(Status::error(""));
                span.set_attribute(KeyValue::new(ERROR_TYPE, error.to_string()));
                span.record_error_ext(error as &dyn Error);
                tracing::error!(error = ?error, "client received error");
            }
        };

        span.end();
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use reqwest_middleware::ClientBuilder;
    use reqwest_middleware::reqwest::Client;
    use reqwest_tracing::OtelPathNames;

    #[tokio::test]
    async fn request_succeeded_no_known_path() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        global::set_text_map_propagator(TraceContextPropagator::new());

        let mut server = mockito::Server::new_async().await;
        let server_mock = server
            .mock("GET", "/hello")
            .match_header("traceparent", mockito::Matcher::Any)
            .with_body("world")
            .create_async()
            .await;

        let client = ClientBuilder::new(Client::new())
            .with(ReqwestTracingMiddleware::new_with_provider(
                provider.clone(),
            ))
            .build();

        let root_span = provider.tracer("tracer").start("test root");
        let cx = Context::current_with_span(root_span);

        let response = client
            .get(format!("{}/hello", server.url()))
            .send()
            .with_context(cx.clone())
            .await
            .unwrap();

        let text = response.text().await.unwrap();
        assert_eq!(text, "world");

        server_mock.assert_async().await;

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let span = &spans[0];

        assert_eq!(
            span.span_context.trace_id(),
            cx.span().span_context().trace_id()
        );
        assert_eq!(span.parent_span_id, cx.span().span_context().span_id());
        assert_eq!(span.span_kind, SpanKind::Client);
        assert_eq!(span.name, "GET");
        assert_eq!(span.status, Status::Unset);
        assert_eq!(span.instrumentation_scope.name(), PKG_NAME);

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(SERVER_ADDRESS, server.socket_address().ip().to_string()),
            KeyValue::new(URL_FULL, format!("{}/hello", server.url())),
            KeyValue::new(SERVER_PORT, server.socket_address().port() as i64),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 200),
        ];
        assert_eq!(span.attributes, attributes);
    }

    #[tokio::test]
    async fn request_succeeded_with_known_path() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        global::set_text_map_propagator(TraceContextPropagator::new());

        let mut server = mockito::Server::new_async().await;
        let server_mock = server
            .mock("GET", "/hello")
            .match_header("traceparent", mockito::Matcher::Any)
            .with_body("world")
            .create_async()
            .await;

        let client = ClientBuilder::new(Client::new())
            .with(ReqwestTracingMiddleware::new_with_provider(
                provider.clone(),
            ))
            .build();

        let root_span = provider.tracer("tracer").start("test root");
        let cx = Context::current_with_span(root_span);

        let response = client
            .get(format!("{}/hello", server.url()))
            .with_extension(OtelPathNames::known_paths(["/hello"]).unwrap())
            .send()
            .with_context(cx.clone())
            .await
            .unwrap();

        let text = response.text().await.unwrap();
        assert_eq!(text, "world");

        server_mock.assert_async().await;

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let span = &spans[0];

        assert_eq!(
            span.span_context.trace_id(),
            cx.span().span_context().trace_id()
        );
        assert_eq!(span.parent_span_id, cx.span().span_context().span_id());
        assert_eq!(span.span_kind, SpanKind::Client);
        assert_eq!(span.name, "GET /hello");
        assert_eq!(span.status, Status::Unset);
        assert_eq!(span.instrumentation_scope.name(), PKG_NAME);

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(SERVER_ADDRESS, server.socket_address().ip().to_string()),
            KeyValue::new(URL_FULL, format!("{}/hello", server.url())),
            KeyValue::new(SERVER_PORT, server.socket_address().port() as i64),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 200),
        ];
        assert_eq!(span.attributes, attributes);
    }

    #[tokio::test]
    async fn request_failed_status_400() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        global::set_text_map_propagator(TraceContextPropagator::new());

        let mut server = mockito::Server::new_async().await;
        let server_mock = server
            .mock("GET", "/hello")
            .match_header("traceparent", mockito::Matcher::Any)
            .with_status(400)
            .with_body("world")
            .create_async()
            .await;

        let client = ClientBuilder::new(Client::new())
            .with(ReqwestTracingMiddleware::new_with_provider(
                provider.clone(),
            ))
            .build();

        let root_span = provider.tracer("tracer").start("test root");
        let cx = Context::current_with_span(root_span);

        let response = client
            .get(format!("{}/hello", server.url()))
            .with_extension(OtelPathNames::known_paths(["/hello"]).unwrap())
            .send()
            .with_context(cx.clone())
            .await
            .unwrap();

        let text = response.text().await.unwrap();
        assert_eq!(text, "world");

        server_mock.assert_async().await;

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let span = &spans[0];

        assert_eq!(
            span.span_context.trace_id(),
            cx.span().span_context().trace_id()
        );
        assert_eq!(span.parent_span_id, cx.span().span_context().span_id());
        assert_eq!(span.span_kind, SpanKind::Client);
        assert_eq!(span.name, "GET /hello");
        assert_eq!(span.status, Status::error(""));
        assert_eq!(span.instrumentation_scope.name(), PKG_NAME);

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(SERVER_ADDRESS, server.socket_address().ip().to_string()),
            KeyValue::new(URL_FULL, format!("{}/hello", server.url())),
            KeyValue::new(SERVER_PORT, server.socket_address().port() as i64),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 400),
            KeyValue::new(ERROR_TYPE, "400"),
        ];
        assert_eq!(span.attributes, attributes);
    }

    #[tokio::test]
    async fn request_failed_status_500() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        global::set_text_map_propagator(TraceContextPropagator::new());

        let mut server = mockito::Server::new_async().await;
        let server_mock = server
            .mock("GET", "/hello")
            .match_header("traceparent", mockito::Matcher::Any)
            .with_status(500)
            .with_body("world")
            .create_async()
            .await;

        let client = ClientBuilder::new(Client::new())
            .with(ReqwestTracingMiddleware::new_with_provider(
                provider.clone(),
            ))
            .build();

        let root_span = provider.tracer("tracer").start("test root");
        let cx = Context::current_with_span(root_span);

        let response = client
            .get(format!("{}/hello", server.url()))
            .with_extension(OtelPathNames::known_paths(["/hello"]).unwrap())
            .send()
            .with_context(cx.clone())
            .await
            .unwrap();

        let text = response.text().await.unwrap();
        assert_eq!(text, "world");

        server_mock.assert_async().await;

        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let span = &spans[0];

        assert_eq!(
            span.span_context.trace_id(),
            cx.span().span_context().trace_id()
        );
        assert_eq!(span.parent_span_id, cx.span().span_context().span_id());
        assert_eq!(span.span_kind, SpanKind::Client);
        assert_eq!(span.name, "GET /hello");
        assert_eq!(span.status, Status::error(""));
        assert_eq!(span.instrumentation_scope.name(), PKG_NAME);

        let attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(SERVER_ADDRESS, server.socket_address().ip().to_string()),
            KeyValue::new(URL_FULL, format!("{}/hello", server.url())),
            KeyValue::new(SERVER_PORT, server.socket_address().port() as i64),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, 500),
            KeyValue::new(ERROR_TYPE, "500"),
        ];
        assert_eq!(span.attributes, attributes);
    }
}

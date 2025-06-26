use crate::PKG_NAME;
use axum::http::Extensions;
use opentelemetry::context::FutureExt;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer};
use opentelemetry::{Context, KeyValue, global};
use opentelemetry_http::HeaderInjector;
use opentelemetry_semantic_conventions::trace::{
    ERROR_TYPE, HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, SERVER_ADDRESS, SERVER_PORT,
    URL_FULL,
};
use reqwest_middleware::reqwest::{Request, Response};
use reqwest_middleware::{Middleware, Next};
use reqwest_tracing::default_span_name;

// TODO: add tests
pub struct ReqwestTracingMiddleware;

#[async_trait::async_trait]
impl Middleware for ReqwestTracingMiddleware {
    async fn handle(
        &self,
        mut req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        global::get_text_map_propagator(|propagator| {
            propagator.inject(&mut HeaderInjector(req.headers_mut()))
        });

        let tracer = global::tracer(PKG_NAME);
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
                span.record_error(&error);
                let err = error.to_string();
                span.set_status(Status::error(err.clone()));
                span.set_attribute(KeyValue::new(ERROR_TYPE, err));
                tracing::error!(error = ?error, "client received error");
            }
        };

        span.end();
        res
    }
}

use crate::PKG_NAME;
use axum::http::Extensions;
use opentelemetry::context::FutureExt;
use opentelemetry::trace::{SpanKind, TraceContextExt, Tracer};
use opentelemetry::{Context, global};
use opentelemetry_http::HeaderInjector;
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
        let span = tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Client)
            .start(&tracer);
        let cx = Context::current_with_span(span);
        let res = next.run(req, extensions).with_context(cx).await;
        res
    }
}

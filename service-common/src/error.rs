use crate::SpanExt;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use opentelemetry::Context;
use opentelemetry::trace::TraceContextExt;

pub struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        Context::map_current(|cx| {
            let span = cx.span();
            span.record_error_ext(&self.0);
        });
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

// support converting Result<_, anyhow::Error> to Result<_, AppError> with `?`
impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(value: E) -> Self {
        Self(value.into())
    }
}

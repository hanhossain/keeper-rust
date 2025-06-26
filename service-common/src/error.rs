use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use opentelemetry::trace::TraceContextExt;
use opentelemetry::{Context, KeyValue};
use opentelemetry_semantic_conventions::trace::{EXCEPTION_MESSAGE, EXCEPTION_STACKTRACE};

pub struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        Context::map_current(|cx| {
            let span = cx.span();
            let attributes = vec![
                KeyValue::new(EXCEPTION_MESSAGE, self.0.to_string()),
                KeyValue::new(EXCEPTION_STACKTRACE, format!("{:?}", self.0)),
            ];

            span.add_event("exception", attributes);
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

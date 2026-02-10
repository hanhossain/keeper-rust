pub mod error;
pub mod middleware;

use axum::http::Version;
use opentelemetry::KeyValue;
use opentelemetry::trace::SpanRef;
use opentelemetry_semantic_conventions::trace::{EXCEPTION_MESSAGE, EXCEPTION_STACKTRACE};
use std::error::Error;
use tokio::signal;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");

pub async fn shutdown_signal() {
    let ctrl_c = async { signal::ctrl_c().await.unwrap() };
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .unwrap()
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {}
    }
}

fn http_version(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "0.9",
        Version::HTTP_10 => "1.0",
        Version::HTTP_11 => "1.1",
        Version::HTTP_2 => "2.0",
        Version::HTTP_3 => "3.0",
        _ => unreachable!(),
    }
}

pub trait SpanExt<T> {
    fn record_error_ext(&self, err: T);
}

impl SpanExt<&dyn Error> for SpanRef<'_> {
    fn record_error_ext(&self, err: &dyn Error) {
        let attributes = vec![
            KeyValue::new(EXCEPTION_MESSAGE, err.to_string()),
            KeyValue::new(EXCEPTION_STACKTRACE, format!("{:#?}", err)),
        ];
        self.add_event("exception", attributes);
    }
}

impl SpanExt<&anyhow::Error> for SpanRef<'_> {
    fn record_error_ext(&self, err: &anyhow::Error) {
        let attributes = vec![
            KeyValue::new(EXCEPTION_MESSAGE, err.to_string()),
            KeyValue::new(EXCEPTION_STACKTRACE, format!("{:?}", err)),
        ];
        self.add_event("exception", attributes);
    }
}

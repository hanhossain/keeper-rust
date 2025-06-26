pub mod error;
pub mod middleware;

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

pub trait SpanExt {
    fn record_error_ext(&self, err: &dyn Error);
}

impl SpanExt for SpanRef<'_> {
    fn record_error_ext(&self, err: &dyn Error) {
        let attributes = vec![
            KeyValue::new(EXCEPTION_MESSAGE, err.to_string()),
            KeyValue::new(EXCEPTION_STACKTRACE, format!("{:?}", err)),
        ];
        self.add_event("exception", attributes);
    }
}

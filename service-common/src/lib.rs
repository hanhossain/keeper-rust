pub mod error;
pub mod middleware;

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

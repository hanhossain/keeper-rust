use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

#[tokio::main]
async fn main() {
    let app = Router::new().route("/ping", get(ping));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ping() -> (StatusCode, Json<Ping>) {
    (
        StatusCode::OK,
        Json(Ping {
            ping: "Pong".to_string(),
        }),
    )
}

#[derive(Serialize)]
struct Ping {
    ping: String,
}

//! The exporter's HTTP surface: two routes, no auth, no secrets.
//!
//! There is deliberately no authentication here. The exporter holds no
//! credential and serves nothing but host power numbers, so a key would be a
//! secret to protect rather than a secret protecting something — and the
//! console would then have to send it, which is a credential the console does
//! not need to hold. What bounds exposure instead is the bind address:
//! loopback by default, and any wider bind is an explicit operator choice
//! (`--listen`) that the exporter announces at startup.

use std::future::Future;
use std::sync::Arc;

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::expo;
use crate::exporter::Exporter;

pub fn router(exporter: Arc<Exporter>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        // No fallback: an unknown path gets axum's empty-body 404, the same
        // as the console and the engine.
        .with_state(exporter)
}

/// Serve until `shutdown` resolves. The binary and the tests both go through
/// here, so the tested wiring is the shipped wiring.
pub async fn serve<F>(
    listener: tokio::net::TcpListener,
    exporter: Arc<Exporter>,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router(exporter))
        .with_graceful_shutdown(shutdown)
        .await
}

async fn metrics(State(exporter): State<Arc<Exporter>>) -> Response {
    let scrape = exporter.scrape().await;
    let body = expo::render(&scrape, exporter.host(), exporter.max_age());
    (
        [(CONTENT_TYPE, HeaderValue::from_static(expo::CONTENT_TYPE))],
        body,
    )
        .into_response()
}

/// Liveness only: it says this process is answering, and claims nothing about
/// whether powermetrics is readable. That question is `muser_agent_up`.
async fn healthz() -> Response {
    (
        [(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        )],
        "{\"ok\":true}",
    )
        .into_response()
}

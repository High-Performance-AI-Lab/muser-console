//! The exporter's HTTP surface: two routes, no auth, no secrets.
//!
//! There is deliberately nothing to authenticate here. The exporter holds
//! no credential — not the engine's, not the console's — and publishes only
//! GPU counters, so a bearer check would protect nothing while giving the
//! console a reason to send it a key. The console is documented never to
//! send one. Access control for a non-loopback bind belongs to the network,
//! which is why the default bind is loopback and a wider one has to be
//! spelled out on the command line.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::header::CONTENT_TYPE as CONTENT_TYPE_HEADER;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use crate::expo;
use crate::logging::log;
use crate::source::GpuSource;

/// Shared handler state: the source, plus enough memory to log a source
/// going up or down once instead of once per scrape. That memory is used
/// for logging only — it is never a fallback value for the exposition.
#[derive(Clone)]
pub struct ExporterState {
    source: Arc<dyn GpuSource>,
    last_up: Arc<Mutex<Option<bool>>>,
}

impl ExporterState {
    pub fn new(source: Arc<dyn GpuSource>) -> ExporterState {
        ExporterState {
            source,
            last_up: Arc::new(Mutex::new(None)),
        }
    }

    /// Logs a transition, and only a transition.
    fn note_state(&self, up: bool, reason: Option<&str>) {
        let mut last = self
            .last_up
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *last == Some(up) {
            return;
        }
        *last = Some(up);
        match reason {
            Some(reason) => log(&format!(
                "NVML unavailable, publishing muser_agent_up 0 and no device series: {reason}"
            )),
            None => log("NVML answering; device series are publishing"),
        }
    }
}

pub fn router(state: ExporterState) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        // No fallback: an unregistered path gets axum's default empty-body
        // 404, matching the console server's route table.
        .with_state(state)
}

async fn metrics(State(state): State<ExporterState>) -> Response {
    let source = state.source.clone();
    // NVML entry points are blocking C calls, so they run on the blocking
    // pool. The duration is measured around the scrape itself, not around
    // the await, so it reports time spent in NVML rather than time spent
    // waiting for a worker.
    let scraped = tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let outcome = source.scrape();
        (outcome, started.elapsed())
    })
    .await;

    let (outcome, elapsed) = match scraped {
        Ok(pair) => pair,
        // The blocking task panicked or the pool is shutting down. Either
        // way the source did not answer this scrape, which is a down
        // scrape — not a zeroed one.
        Err(error) => (Err(format!("scrape task failed: {error}")), Duration::ZERO),
    };

    state.note_state(outcome.is_ok(), outcome.as_ref().err().map(String::as_str));

    let body = expo::render(outcome.as_deref().ok(), elapsed);
    ([(CONTENT_TYPE_HEADER, expo::CONTENT_TYPE)], body).into_response()
}

/// Liveness of this process, and nothing more.
///
/// It stays `ok` while NVML is unavailable on purpose: whether the GPU
/// source answered is `muser_agent_up`, and letting a health check fail on
/// absent hardware would make a supervisor restart a perfectly healthy
/// exporter that is honestly reporting a gap.
async fn healthz() -> Response {
    Json(serde_json::json!({"ok": true})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{DeviceSample, RecordedSource};

    #[test]
    fn a_state_change_is_logged_once_not_every_scrape() {
        let state = ExporterState::new(Arc::new(RecordedSource::devices(vec![DeviceSample::new(
            0,
        )])));
        // Only the transitions are observable side effects; what this pins
        // is that the bookkeeping itself is idempotent and never becomes a
        // source of values.
        state.note_state(true, None);
        state.note_state(true, None);
        assert_eq!(
            *state
                .last_up
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            Some(true)
        );
        state.note_state(false, Some("NVML absent"));
        assert_eq!(
            *state
                .last_up
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            Some(false)
        );
    }
}

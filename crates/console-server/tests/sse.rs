//! SSE fidelity: /telemetry must stream chunk-by-chunk. The stub emits three
//! `event: snapshot` frames 150 ms apart carrying a literal live snapshot;
//! the client must observe frame 1 before frame 3 is even produced.

mod common;

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::header::CONTENT_TYPE;
use axum::routing::get;
use axum::Router;
use http_body_util::{BodyExt as _, Full};

#[tokio::test]
async fn telemetry_frames_arrive_incrementally() {
    let snapshot = common::telemetry_snapshot_compact();
    let frame_text = format!("event: snapshot\ndata: {snapshot}\n\n");
    let frame3_sent: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

    let stub_frame = frame_text.clone();
    let stub_sent = Arc::clone(&frame3_sent);
    let stub = Router::new().route(
        "/telemetry",
        get(move || {
            let frame = stub_frame.clone();
            let sent = Arc::clone(&stub_sent);
            async move {
                let stream = futures_util::stream::unfold(0usize, move |index| {
                    let frame = frame.clone();
                    let sent = Arc::clone(&sent);
                    async move {
                        if index >= 3 {
                            return None;
                        }
                        if index > 0 {
                            tokio::time::sleep(Duration::from_millis(150)).await;
                        }
                        if index == 2 {
                            *sent.lock().expect("stub lock") = Some(Instant::now());
                        }
                        Some((Ok::<_, Infallible>(Bytes::from(frame)), index + 1))
                    }
                });
                (
                    [(CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(stream),
                )
            }
        }),
    );
    let upstream = common::spawn_router(stub).await;
    let console = common::spawn_console(upstream).await;
    let client = common::client();

    let request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("http://{console}/telemetry"))
        .header("authorization", common::bearer(common::CONSOLE_KEY))
        .body(Full::new(Bytes::new()))
        .expect("build request");
    let response = client.request(request).await.expect("request");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).expect("content-type"),
        "text/event-stream"
    );

    let mut body = response.into_body();
    let mut received: Vec<u8> = Vec::new();
    let mut first_seen: Option<Instant> = None;
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("body frame");
        if let Some(data) = frame.data_ref() {
            if first_seen.is_none() && !data.is_empty() {
                first_seen = Some(Instant::now());
            }
            received.extend_from_slice(data);
        }
    }

    let first_seen = first_seen.expect("received at least one chunk");
    let frame3_sent = frame3_sent
        .lock()
        .expect("lock")
        .expect("stub produced frame 3");
    assert!(
        first_seen < frame3_sent,
        "first SSE frame must be observed before frame 3 is produced \
         (proxy buffered the stream: first seen {:?} after frame 3 at {:?})",
        first_seen,
        frame3_sent
    );
    let expected = frame_text.repeat(3);
    assert!(
        received == expected.as_bytes(),
        "all three frames must pass through byte-exact"
    );
}

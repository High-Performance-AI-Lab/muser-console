//! Phase-2 fleet surface: /v1/fleet shape + auth, the /i/{name} namespace
//! (prefix stripping, per-instance keys, unknown-name 404), instance-scoped
//! WS tickets, root-route compat, and the isolation acceptance test (a dead
//! instance must not disturb a live one, including an in-flight SSE stream).
//! Stub payloads are literal live snapshot bytes; WS routing checks use opaque
//! plumbing markers only.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::Request;
use axum::http::header::CONTENT_TYPE;
use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::Router;
use futures_util::StreamExt as _;
use http_body_util::{BodyExt as _, Full};
use tokio_tungstenite::tungstenite;

type RecordedRequest = (String, String, Option<String>);
type RequestLog = Arc<Mutex<Vec<RecordedRequest>>>;

// ---------------------------------------------------------------------------
// /v1/fleet

#[tokio::test]
async fn fleet_shape_and_auth() {
    // The fleet listing never contacts an upstream; dead addresses suffice.
    let dead_a: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let dead_b: SocketAddr = "127.0.0.1:2".parse().expect("addr");
    let console = common::spawn_console_fleet(&[("gx", dead_a), ("mac", dead_b)]).await;
    let client = common::client();

    // Exact 401 parity with every other console-authenticated route.
    for headers in [
        &[][..],
        &[("authorization", "Bearer definitely-not-the-key")][..],
    ] {
        let (parts, body) =
            common::request(&client, "GET", console, "/v1/fleet", headers, b"").await;
        assert_eq!(parts.status, 401);
        assert_eq!(
            parts
                .headers
                .get("www-authenticate")
                .expect("www-authenticate header"),
            "Bearer"
        );
        assert_eq!(
            body.as_ref(),
            common::engine_error_body(
                "authentication_required",
                "a valid bearer API key is required"
            )
        );
    }

    let (parts, body) = common::request(
        &client,
        "GET",
        console,
        "/v1/fleet",
        &[("authorization", &common::bearer(common::CONSOLE_KEY))],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200);
    let value: serde_json::Value = serde_json::from_slice(&body).expect("fleet body parses");
    assert_eq!(
        value,
        serde_json::json!({
            "instances": [
                {"name": "gx", "authority": "127.0.0.1:1", "default": true, "agents": []},
                {"name": "mac", "authority": "127.0.0.1:2", "default": false, "agents": []},
            ]
        }),
        "config order, exactly one default (the first); no agents configured here"
    );
}

// ---------------------------------------------------------------------------
// Unknown instance name

#[tokio::test]
async fn unknown_instance_gets_console_404_envelope() {
    let dead: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let console = common::spawn_console_fleet(&[("gx", dead)]).await;
    let client = common::client();
    let auth = common::bearer(common::CONSOLE_KEY);
    let expected = common::engine_error_body("not_found", "unknown instance");

    for (method, path) in [
        ("GET", "/i/nope/snapshot"),
        ("GET", "/i/nope/metrics"),
        ("GET", "/i/nope/telemetry"),
        ("GET", "/i/nope/v1/nodes"),
        ("POST", "/i/nope/v1/nodes"),
        ("GET", "/i/nope/v1/nodes/n1/progress"),
        ("POST", "/i/nope/v1/ws-tickets"),
    ] {
        let (parts, body) = common::request(
            &client,
            method,
            console,
            path,
            &[
                ("authorization", &auth),
                ("content-type", "application/json"),
            ],
            b"{}",
        )
        .await;
        assert_eq!(parts.status, 404, "{method} {path}");
        assert_eq!(body.as_ref(), expected, "{method} {path}");
    }

    // Bearer parity comes first on bearer-authenticated routes: an
    // unauthenticated probe cannot enumerate instance names via 401/404.
    let (parts, body) =
        common::request(&client, "GET", console, "/i/nope/snapshot", &[], b"").await;
    assert_eq!(parts.status, 401);
    assert_eq!(
        body.as_ref(),
        common::engine_error_body(
            "authentication_required",
            "a valid bearer API key is required"
        )
    );

    // /stream's credential is the ticket itself, so an unknown name fails
    // exactly like a bogus ticket: 401, never a name-revealing 404.
    let error = tokio_tungstenite::connect_async(format!("ws://{console}/i/nope/stream?ticket=x"))
        .await
        .expect_err("unknown instance stream must be rejected");
    assert_handshake_status(error, 401);
}

// ---------------------------------------------------------------------------
// Prefix stripping + per-instance credential isolation + root compat

#[derive(Clone)]
struct RecordingStub {
    name: &'static str,
    /// (method, path-and-query, authorization) per request, in order.
    seen: RequestLog,
}

fn recording_stub(stub: RecordingStub) -> Router {
    Router::new().fallback(move |request: Request| {
        let stub = stub.clone();
        async move {
            let method = request.method().to_string();
            let path = request
                .uri()
                .path_and_query()
                .map_or("/", |value| value.as_str())
                .to_owned();
            let authorization = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            stub.seen
                .lock()
                .expect("stub lock")
                .push((method, path, authorization));
            (
                [(CONTENT_TYPE, "application/json")],
                format!("{{\"stub\":\"{}\"}}", stub.name),
            )
        }
    })
}

#[tokio::test]
async fn instance_prefix_is_stripped_and_keys_stay_per_instance() {
    let seen_a = Arc::new(Mutex::new(Vec::new()));
    let seen_b = Arc::new(Mutex::new(Vec::new()));
    let a = common::spawn_router(recording_stub(RecordingStub {
        name: "a",
        seen: Arc::clone(&seen_a),
    }))
    .await;
    let b = common::spawn_router(recording_stub(RecordingStub {
        name: "b",
        seen: Arc::clone(&seen_b),
    }))
    .await;
    let console = common::spawn_console_fleet(&[("a", a), ("b", b)]).await;
    let client = common::client();
    let auth = common::bearer(common::CONSOLE_KEY);

    for (method, console_path, marker) in [
        ("GET", "/i/a/snapshot", "a"),
        ("GET", "/i/b/metrics", "b"),
        ("GET", "/i/a/v1/nodes/n1/progress", "a"),
        ("POST", "/i/b/v1/nodes", "b"),
        // Root-anchored phase-1 routes keep hitting the default instance.
        ("GET", "/snapshot", "a"),
        ("GET", "/v1/nodes/n1/progress", "a"),
    ] {
        let (parts, body) = common::request(
            &client,
            method,
            console,
            console_path,
            &[
                ("authorization", &auth),
                ("content-type", "application/json"),
            ],
            b"{}",
        )
        .await;
        assert_eq!(parts.status, 200, "{method} {console_path}");
        let expected = format!("{{\"stub\":\"{marker}\"}}");
        assert_eq!(
            body.as_ref(),
            expected.as_bytes(),
            "{method} {console_path} must reach stub '{marker}'"
        );
    }

    let recorded_a = seen_a.lock().expect("lock").clone();
    let recorded_b = seen_b.lock().expect("lock").clone();
    assert_eq!(
        recorded_a
            .iter()
            .map(|(method, path, _)| (method.as_str(), path.as_str()))
            .collect::<Vec<_>>(),
        [
            ("GET", "/snapshot"),
            ("GET", "/v1/nodes/n1/progress"),
            ("GET", "/snapshot"),
            ("GET", "/v1/nodes/n1/progress"),
        ],
        "the /i/{{name}} prefix must be stripped before forwarding"
    );
    assert_eq!(
        recorded_b
            .iter()
            .map(|(method, path, _)| (method.as_str(), path.as_str()))
            .collect::<Vec<_>>(),
        [("GET", "/metrics"), ("POST", "/v1/nodes")],
    );
    for (name, recorded) in [("a", &recorded_a), ("b", &recorded_b)] {
        let expected = common::bearer(&common::instance_key(name));
        for (method, path, authorization) in recorded {
            let authorization = authorization.as_deref().expect("engine bearer injected");
            assert!(
                authorization == expected,
                "{method} {path}: instance '{name}' must see its own engine key"
            );
            assert!(
                !authorization.contains(common::CONSOLE_KEY),
                "console key must never reach an engine"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Isolation acceptance: a dead instance leaves the others unaffected

#[tokio::test]
async fn dead_instance_leaves_live_instance_unaffected() {
    let fixture_a = common::live_engine_snapshot_bytes();
    let fixture_b = common::live_console_snapshot_bytes();
    assert!(
        fixture_a != fixture_b,
        "isolation test requires two different live snapshot payloads"
    );

    let snapshot = common::telemetry_snapshot_compact();
    let frame = format!("event: snapshot\ndata: {snapshot}\n\n");

    // Stub A: /snapshot with fixture A, /telemetry an unbounded SSE stream
    // emitting the real captured delta every 100 ms.
    let snapshot_a = fixture_a.clone();
    let stream_frame = frame.clone();
    let stub_a = Router::new()
        .route(
            "/snapshot",
            get(move || {
                let body = snapshot_a.clone();
                async move { ([(CONTENT_TYPE, "application/json")], body) }
            }),
        )
        .route(
            "/telemetry",
            get(move || {
                let frame = stream_frame.clone();
                async move {
                    let stream = futures_util::stream::unfold(0u64, move |tick| {
                        let frame = frame.clone();
                        async move {
                            if tick > 0 {
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                            Some((
                                Ok::<_, std::convert::Infallible>(Bytes::from(frame)),
                                tick + 1,
                            ))
                        }
                    });
                    (
                        [(CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(stream),
                    )
                }
            }),
        );
    let a = common::spawn_router(stub_a).await;

    // Stub B: fixture B, spawned stoppable so the test can kill it.
    let snapshot_b = fixture_b.clone();
    let stub_b = Router::new().route(
        "/snapshot",
        get(move || {
            let body = snapshot_b.clone();
            async move { ([(CONTENT_TYPE, "application/json")], body) }
        }),
    );
    let (b, b_task) = common::spawn_router_stoppable(stub_b).await;

    let console = common::spawn_console_fleet(&[("a", a), ("b", b)]).await;
    let client = common::client();
    let auth = common::bearer(common::CONSOLE_KEY);

    // Each instance serves its own bytes; the root routes map to the default.
    for (path, expected) in [
        ("/i/a/snapshot", &fixture_a),
        ("/i/b/snapshot", &fixture_b),
        ("/snapshot", &fixture_a),
    ] {
        let (parts, body) = common::request(
            &client,
            "GET",
            console,
            path,
            &[("authorization", &auth)],
            b"",
        )
        .await;
        assert_eq!(parts.status, 200, "GET {path}");
        assert!(
            body.as_ref() == expected.as_slice(),
            "GET {path} must return its own instance's fixture bytes"
        );
    }

    // Open a live SSE stream on instance A and observe a frame arriving.
    let request = axum::http::Request::builder()
        .method("GET")
        .uri(format!("http://{console}/i/a/telemetry"))
        .header("authorization", &auth)
        .body(Full::new(Bytes::new()))
        .expect("build request");
    let response = client.request(request).await.expect("telemetry request");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .expect("sse content-type"),
        "text/event-stream"
    );
    let mut sse_body = response.into_body();
    let mut received: Vec<u8> = Vec::new();
    pump_sse(&mut sse_body, &mut received, frame.len(), 1).await;

    // Kill instance B outright: listener and in-flight connections drop.
    b_task.abort();

    // /i/b -> 502 upstream_unreachable envelope (poll briefly for the abort
    // to land; afterwards it must answer 502 immediately).
    let expected_502 =
        common::engine_error_body("upstream_unreachable", "instance 'b' is unreachable");
    let mut last: Option<(u16, Bytes)> = None;
    for _ in 0..100 {
        let (parts, body) = common::request(
            &client,
            "GET",
            console,
            "/i/b/snapshot",
            &[("authorization", &auth)],
            b"",
        )
        .await;
        let status = parts.status.as_u16();
        last = Some((status, body));
        if status == 502 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (status, body) = last.expect("at least one /i/b request ran");
    assert_eq!(status, 502, "dead instance must answer 502");
    assert_eq!(body.as_ref(), expected_502);

    // Instance A is unaffected: buffered route still serves its bytes...
    let (parts, body) = common::request(
        &client,
        "GET",
        console,
        "/i/a/snapshot",
        &[("authorization", &auth)],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200, "live instance must keep answering");
    assert!(
        body.as_ref() == fixture_a.as_slice(),
        "live instance must keep returning its own bytes while B is dead"
    );

    // ...and the SSE stream opened before B died keeps delivering frames.
    pump_sse(&mut sse_body, &mut received, frame.len(), 4).await;
    let bound = frame.repeat(received.len() / frame.len() + 1);
    assert!(
        bound.as_bytes().starts_with(&received),
        "in-flight SSE bytes must stay byte-exact repeated frames"
    );
    drop(sse_body);
}

/// Read chunks until at least `minimum` complete frames of `frame_len`
/// bytes have accumulated. Bounded by a 10 s timeout so a stalled proxy
/// fails the test instead of hanging it.
async fn pump_sse(
    body: &mut hyper::body::Incoming,
    received: &mut Vec<u8>,
    frame_len: usize,
    minimum: usize,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while received.len() / frame_len < minimum {
            let next = body
                .frame()
                .await
                .expect("SSE stream must not end")
                .expect("SSE body frame ok");
            if let Some(data) = next.data_ref() {
                received.extend_from_slice(data);
            }
        }
    })
    .await
    .expect("SSE frames must keep arriving");
}

// ---------------------------------------------------------------------------
// Instance-scoped WS tickets

const ENGINE_TICKET: &str = "engine-ticket-fleet";

/// Minimal engine WS stub: mints a fixed engine ticket and, on a correctly
/// ticketed /stream, sends one opaque plumbing marker identifying the stub.
fn ws_stub(marker: &'static str) -> Router {
    Router::new()
        .route(
            "/v1/ws-tickets",
            post(|| async {
                (
                    [(CONTENT_TYPE, "application/json")],
                    format!(
                        "{{\"ticket\":\"{ENGINE_TICKET}\",\"expires_in\":30,\"single_use\":true}}"
                    ),
                )
            }),
        )
        .route(
            "/stream",
            any(move |websocket: WebSocketUpgrade, uri: Uri| async move {
                if uri.query() != Some(&format!("ticket={ENGINE_TICKET}")[..]) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                websocket
                    .on_upgrade(move |mut socket| async move {
                        let _ = socket.send(Message::Text(marker.into())).await;
                        while let Some(Ok(message)) = socket.recv().await {
                            if matches!(message, Message::Close(_)) {
                                break;
                            }
                        }
                    })
                    .into_response()
            }),
        )
}

async fn mint_ticket(client: &common::TestClient, console: SocketAddr, path: &str) -> String {
    let (parts, body) = common::request(
        client,
        "POST",
        console,
        path,
        &[("authorization", &common::bearer(common::CONSOLE_KEY))],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200, "POST {path}");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("ticket response parses");
    value
        .get("ticket")
        .and_then(|v| v.as_str())
        .expect("ticket present")
        .to_owned()
}

async fn expect_marker(console: SocketAddr, path_and_ticket: &str, marker: &str) {
    let (mut socket, _response) =
        tokio_tungstenite::connect_async(format!("ws://{console}{path_and_ticket}"))
            .await
            .expect("bridge handshake must succeed");
    let first = socket
        .next()
        .await
        .expect("first frame present")
        .expect("first frame ok");
    assert_eq!(
        first.into_text().expect("first frame is text").as_str(),
        marker,
        "bridge must reach the instance the ticket was minted for"
    );
    let _ = socket.close(None).await;
}

#[tokio::test]
async fn tickets_are_instance_scoped() {
    let a = common::spawn_router(ws_stub("upstream-a")).await;
    let b = common::spawn_router(ws_stub("upstream-b")).await;
    let console = common::spawn_console_fleet(&[("a", a), ("b", b)]).await;
    let client = common::client();

    // A ticket minted for instance a is not a credential for instance b.
    let ticket = mint_ticket(&client, console, "/i/a/v1/ws-tickets").await;
    let error =
        tokio_tungstenite::connect_async(format!("ws://{console}/i/b/stream?ticket={ticket}"))
            .await
            .expect_err("cross-instance ticket use must be rejected");
    assert_handshake_status(error, 401);

    // Root-minted tickets belong to the default instance only.
    let root_ticket = mint_ticket(&client, console, "/v1/ws-tickets").await;
    let error =
        tokio_tungstenite::connect_async(format!("ws://{console}/i/b/stream?ticket={root_ticket}"))
            .await
            .expect_err("root ticket on a non-default instance must be rejected");
    assert_handshake_status(error, 401);

    // Scoped tickets bridge to their own instance's engine.
    let ticket_a = mint_ticket(&client, console, "/i/a/v1/ws-tickets").await;
    expect_marker(
        console,
        &format!("/i/a/stream?ticket={ticket_a}"),
        "upstream-a",
    )
    .await;
    let ticket_b = mint_ticket(&client, console, "/i/b/v1/ws-tickets").await;
    expect_marker(
        console,
        &format!("/i/b/stream?ticket={ticket_b}"),
        "upstream-b",
    )
    .await;

    // Root /stream still bridges to the default instance.
    let root_ticket = mint_ticket(&client, console, "/v1/ws-tickets").await;
    expect_marker(
        console,
        &format!("/stream?ticket={root_ticket}"),
        "upstream-a",
    )
    .await;
}

fn assert_handshake_status(error: tungstenite::Error, expected: u16) {
    match error {
        tungstenite::Error::Http(response) => {
            assert_eq!(response.status().as_u16(), expected);
        }
        other => panic!("expected HTTP handshake rejection, got {other:?}"),
    }
}

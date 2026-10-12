//! `/stream` bridge coverage includes console ticket minting, server-side
//! engine ticket exchange, and frame forwarding. The stub engine's hello-like
//! frame carries real live `/snapshot` bytes.

mod common;

use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use axum::Router;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite;

const ENGINE_TICKET: &str = "engine-ticket-0001";

#[derive(Clone)]
struct StubState {
    delta: String,
    ticket_request_headers: Arc<Mutex<Option<axum::http::HeaderMap>>>,
}

fn stub_engine(state: StubState) -> Router {
    Router::new()
        .route("/v1/ws-tickets", post(stub_mint))
        .route("/stream", any(stub_stream))
        .with_state(state)
}

async fn stub_mint(State(state): State<StubState>, headers: axum::http::HeaderMap) -> Response {
    *state.ticket_request_headers.lock().expect("stub lock") = Some(headers);
    (
        [(CONTENT_TYPE, "application/json")],
        format!("{{\"ticket\":\"{ENGINE_TICKET}\",\"expires_in\":30,\"single_use\":true}}"),
    )
        .into_response()
}

async fn stub_stream(
    State(state): State<StubState>,
    websocket: WebSocketUpgrade,
    uri: Uri,
) -> Response {
    if uri.query() != Some(&format!("ticket={ENGINE_TICKET}")[..]) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    websocket.on_upgrade(move |mut socket| async move {
        let _ = socket.send(Message::Text(state.delta.clone().into())).await;
        while let Some(Ok(message)) = socket.recv().await {
            match message {
                Message::Text(text) => {
                    if socket.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    })
}

async fn spawn_pair() -> (std::net::SocketAddr, StubState) {
    let state = StubState {
        delta: common::telemetry_snapshot_compact(),
        ticket_request_headers: Arc::new(Mutex::new(None)),
    };
    let upstream = common::spawn_router(stub_engine(state.clone())).await;
    let console = common::spawn_console(upstream).await;
    (console, state)
}

async fn mint_console_ticket(client: &common::TestClient, console: std::net::SocketAddr) -> String {
    let (parts, body) = common::request(
        client,
        "POST",
        console,
        "/v1/ws-tickets",
        &[("authorization", &common::bearer(common::CONSOLE_KEY))],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200);
    let value: serde_json::Value = serde_json::from_slice(&body).expect("ticket response parses");
    assert_eq!(value.get("expires_in").and_then(|v| v.as_i64()), Some(30));
    assert_eq!(
        value.get("single_use").and_then(|v| v.as_bool()),
        Some(true)
    );
    let ticket = value
        .get("ticket")
        .and_then(|v| v.as_str())
        .expect("ticket present")
        .to_owned();
    let literal = std::str::from_utf8(&body).expect("ticket response is UTF-8");
    assert_eq!(
        literal.replace(&ticket, "<ticket>"),
        common::ENGINE_WS_TICKET_SHAPE,
        "console ticket bytes must use the current engine's insertion order"
    );
    ticket
}

#[tokio::test]
async fn mint_requires_console_bearer() {
    let (console, _state) = spawn_pair().await;
    let client = common::client();
    let (parts, body) = common::request(&client, "POST", console, "/v1/ws-tickets", &[], b"").await;
    assert_eq!(parts.status, 401);
    assert_eq!(body.as_ref(), common::ENGINE_AUTH_REQUIRED_BODY);
}

#[tokio::test]
async fn full_bridge_path_forwards_frames() {
    let (console, state) = spawn_pair().await;
    let client = common::client();
    let ticket = mint_console_ticket(&client, console).await;

    let (mut socket, _response) =
        tokio_tungstenite::connect_async(format!("ws://{console}/stream?ticket={ticket}"))
            .await
            .expect("bridge handshake must succeed");

    // Hello-like frame from the stub engine: literal live snapshot bytes.
    let first = socket
        .next()
        .await
        .expect("first frame present")
        .expect("first frame ok");
    let text = first.into_text().expect("first frame is text");
    assert!(
        text.as_str() == state.delta,
        "hello-like frame must arrive byte-exact through the bridge"
    );

    // Round-trip an opaque plumbing frame through the echo stub.
    socket
        .send(tungstenite::Message::Text("bridge-echo-check".into()))
        .await
        .expect("send through bridge");
    let echoed = socket
        .next()
        .await
        .expect("echo frame present")
        .expect("echo frame ok");
    assert_eq!(
        echoed.into_text().expect("echo is text").as_str(),
        "bridge-echo-check"
    );
    socket.close(None).await.expect("close bridge");

    // The server-side ticket exchange must carry the ENGINE bearer and no
    // browser credentials.
    let seen = state
        .ticket_request_headers
        .lock()
        .expect("lock")
        .clone()
        .expect("stub ticket route was hit");
    let authorization = seen.get("authorization").expect("engine bearer present");
    assert!(
        authorization.as_bytes() == common::bearer(common::ENGINE_KEY).as_bytes(),
        "ticket exchange must use the engine instance key"
    );
    assert!(seen.get("cookie").is_none(), "no cookie upstream");
}

#[tokio::test]
async fn ticket_reuse_and_bad_tickets_rejected() {
    let (console, _state) = spawn_pair().await;
    let client = common::client();

    // Consume a ticket, then try to reuse it.
    let ticket = mint_console_ticket(&client, console).await;
    let (mut socket, _response) =
        tokio_tungstenite::connect_async(format!("ws://{console}/stream?ticket={ticket}"))
            .await
            .expect("first use succeeds");
    let _ = socket.next().await;
    drop(socket);

    let reuse = tokio_tungstenite::connect_async(format!("ws://{console}/stream?ticket={ticket}"))
        .await
        .expect_err("ticket reuse must be rejected");
    assert_handshake_status(reuse, 401);

    let bogus = tokio_tungstenite::connect_async(format!(
        "ws://{console}/stream?ticket=bogus-ticket-value"
    ))
    .await
    .expect_err("bad ticket must be rejected");
    assert_handshake_status(bogus, 401);

    let missing = tokio_tungstenite::connect_async(format!("ws://{console}/stream"))
        .await
        .expect_err("missing ticket must be rejected");
    assert_handshake_status(missing, 401);

    // Unknown query params are rejected like the engine's
    // deny_unknown_fields StreamQuery, even with a valid ticket.
    let fresh = mint_console_ticket(&client, console).await;
    let extra =
        tokio_tungstenite::connect_async(format!("ws://{console}/stream?ticket={fresh}&extra=1"))
            .await
            .expect_err("unknown query param must be rejected");
    assert_handshake_body(extra, 400, common::ENGINE_INVALID_STREAM_QUERY_BODY);
}

#[tokio::test]
async fn failed_upstream_exchange_closes_downstream_1008() {
    // Console pointed at a closed port: handshake succeeds, then the console
    // must close with 1008 and never fabricate frames.
    let upstream = "127.0.0.1:1".parse().expect("addr");
    let console = common::spawn_console(upstream).await;
    let client = common::client();
    let ticket = mint_console_ticket(&client, console).await;

    let (mut socket, _response) =
        tokio_tungstenite::connect_async(format!("ws://{console}/stream?ticket={ticket}"))
            .await
            .expect("downstream handshake succeeds");
    let frame = socket.next().await.expect("close frame present");
    match frame.expect("frame ok") {
        tungstenite::Message::Close(Some(close)) => {
            assert_eq!(u16::from(close.code), 1008);
        }
        other => panic!("expected close frame, got {other:?}"),
    }
}

fn assert_handshake_status(error: tungstenite::Error, expected: u16) {
    match error {
        tungstenite::Error::Http(response) => {
            assert_eq!(response.status().as_u16(), expected);
        }
        other => panic!("expected HTTP handshake rejection, got {other:?}"),
    }
}

fn assert_handshake_body(error: tungstenite::Error, expected: u16, body: &[u8]) {
    match error {
        tungstenite::Error::Http(response) => {
            assert_eq!(response.status().as_u16(), expected);
            assert_eq!(response.body().as_deref(), Some(body));
        }
        other => panic!("expected HTTP handshake rejection, got {other:?}"),
    }
}

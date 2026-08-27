//! /stream bridge: console ticket in, engine ticket exchanged server-side,
//! frames forwarded verbatim in both directions. The console never
//! fabricates telemetry frames.

use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::{SinkExt as _, StreamExt as _};
use http_body_util::BodyExt as _;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::protocol::Message as UpstreamMessage;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::auth::{auth_required, error_json, mutation_rejection};
use crate::config::Instance;
use crate::logging::log;
use crate::routes::{split_instance, unknown_instance};
use crate::state::AppState;

/// Overall timeout for the server-side engine ticket exchange.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-frame send timeout inside the bridge: a peer that stops reading must
/// not wedge the bridge task forever (the engine itself drops subscribers
/// after 5 s of backpressure).
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Root mint: a ticket for the default instance (phase-1 compat).
pub async fn mint(State(state): State<AppState>, request: Request) -> Response {
    if let Some(response) = mutation_rejection(&state, request.headers()) {
        return response;
    }
    mint_response(&state, &state.default_instance().name)
}

/// `/i/{name}/v1/ws-tickets`: a ticket scoped to that instance. Bearer is
/// checked before instance resolution, matching the buffered proxy routes.
pub async fn mint_instance(State(state): State<AppState>, request: Request) -> Response {
    if let Some(response) = mutation_rejection(&state, request.headers()) {
        return response;
    }
    let Some(instance) = split_instance(request.uri().path())
        .and_then(|(name, _)| state.instance(name))
        .map(|instance| instance.name.clone())
    else {
        return unknown_instance();
    };
    mint_response(&state, &instance)
}

fn mint_response(state: &AppState, instance_name: &str) -> Response {
    let ticket = match state.mint_ticket(instance_name) {
        Ok(ticket) => ticket,
        Err(_) => {
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "ticket entropy unavailable",
            )
        }
    };
    Json(serde_json::json!({"ticket": ticket, "expires_in": 30, "single_use": true}))
        .into_response()
}

/// Root `/stream`: bridges to the default instance; consumes only tickets
/// minted for it (root mint or `/i/{default}` mint).
pub async fn stream(
    State(state): State<AppState>,
    websocket: WebSocketUpgrade,
    uri: Uri,
) -> Response {
    let instance = state.default_instance().name.clone();
    stream_for(state, websocket, uri, instance)
}

/// `/i/{name}/stream`. The credential here is the instance-scoped ticket,
/// checked against the raw name segment: tickets are only ever minted for
/// configured instances, so an unknown name fails exactly like a bogus
/// ticket (401) — no 401/404 oracle for unauthenticated name enumeration.
pub async fn stream_instance(
    State(state): State<AppState>,
    websocket: WebSocketUpgrade,
    uri: Uri,
) -> Response {
    let Some((name, _)) = split_instance(uri.path()) else {
        return auth_required();
    };
    let name = name.to_owned();
    stream_for(state, websocket, uri, name)
}

fn stream_for(
    state: AppState,
    websocket: WebSocketUpgrade,
    uri: Uri,
    instance_name: String,
) -> Response {
    // Strict query parsing mirrors the engine's deny_unknown_fields
    // StreamQuery; only a single-use console ticket authenticates here.
    let ticket = match parse_stream_query(uri.query()) {
        Ok(ticket) => ticket,
        Err(()) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "unknown query parameter on /stream",
            )
        }
    };
    let authenticated = ticket
        .as_deref()
        .is_some_and(|candidate| state.consume_ticket(candidate, &instance_name));
    if !authenticated {
        return auth_required();
    }
    websocket.on_upgrade(move |socket| bridge(socket, state, instance_name))
}

fn parse_stream_query(query: Option<&str>) -> Result<Option<String>, ()> {
    let Some(query) = query else { return Ok(None) };
    if query.is_empty() {
        return Ok(None);
    }
    let mut ticket = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "ticket" || ticket.is_some() {
            return Err(());
        }
        ticket = Some(value.to_owned());
    }
    Ok(ticket)
}

async fn bridge(mut downstream: WebSocket, state: AppState, instance_name: String) {
    // Resolution cannot fail here — the handler resolved the same immutable
    // config before upgrading — but a close beats a panic in a task.
    let connected = match state.instance(&instance_name) {
        Some(instance) => connect_upstream(&state, instance).await,
        None => Err("instance vanished".to_owned()),
    };
    let upstream = match connected {
        Ok(upstream) => upstream,
        Err(reason) => {
            log(&format!(
                "stream bridge to instance '{instance_name}' failed: {reason}"
            ));
            let _ = downstream
                .send(Message::Close(Some(CloseFrame {
                    code: 1008,
                    reason: "upstream stream unavailable".into(),
                })))
                .await;
            return;
        }
    };

    let (mut upstream_sink, mut upstream_source) = upstream.split();
    let (mut downstream_sink, mut downstream_source) = downstream.split();
    loop {
        tokio::select! {
            inbound = downstream_source.next() => match inbound {
                Some(Ok(message)) => {
                    let closing = matches!(message, Message::Close(_));
                    let sent = timeout(SEND_TIMEOUT, upstream_sink.send(to_upstream(message))).await;
                    if !matches!(sent, Ok(Ok(()))) || closing {
                        break;
                    }
                }
                _ => break,
            },
            inbound = upstream_source.next() => match inbound {
                Some(Ok(message)) => {
                    let closing = matches!(message, UpstreamMessage::Close(_));
                    let Some(converted) = to_downstream(message) else { continue };
                    let sent = timeout(SEND_TIMEOUT, downstream_sink.send(converted)).await;
                    if !matches!(sent, Ok(Ok(()))) || closing {
                        break;
                    }
                }
                _ => break,
            },
        }
    }
    let _ = timeout(SEND_TIMEOUT, upstream_sink.close()).await;
    let _ = timeout(SEND_TIMEOUT, downstream_sink.close()).await;
}

async fn connect_upstream(
    state: &AppState,
    instance: &Instance,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String> {
    let exchange = async {
        let request = Request::builder()
            .method("POST")
            .uri(format!("{}/v1/ws-tickets", instance.base_url))
            .header(AUTHORIZATION, instance.bearer.clone())
            .body(Body::empty())
            .map_err(|error| format!("build ticket request: {error}"))?;
        let response = state
            .client(instance)
            .request(request)
            .await
            .map_err(|error| format!("ticket exchange: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("ticket exchange returned {status}"));
        }
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|error| format!("ticket exchange body: {error}"))?
            .to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("ticket exchange response: {error}"))?;
        value
            .get("ticket")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "ticket exchange response missing ticket".to_owned())
    };
    let ticket = timeout(EXCHANGE_TIMEOUT, exchange)
        .await
        .map_err(|_| "ticket exchange timed out".to_owned())??;

    let scheme = if instance.is_https { "wss" } else { "ws" };
    let url = format!("{scheme}://{}/stream?ticket={ticket}", instance.authority);
    let connector = if instance.is_https {
        Connector::Rustls(instance.tls_config.clone())
    } else {
        Connector::Plain
    };
    let connected =
        tokio_tungstenite::connect_async_tls_with_config(url, None, true, Some(connector));
    let (stream, _response) = timeout(EXCHANGE_TIMEOUT, connected)
        .await
        .map_err(|_| "upstream websocket connect timed out".to_owned())?
        .map_err(|error| format!("upstream websocket connect: {error}"))?;
    Ok(stream)
}

fn to_upstream(message: Message) -> UpstreamMessage {
    match message {
        Message::Text(text) => UpstreamMessage::Text(text.as_str().to_owned().into()),
        Message::Binary(data) => UpstreamMessage::Binary(data),
        Message::Ping(data) => UpstreamMessage::Ping(data),
        Message::Pong(data) => UpstreamMessage::Pong(data),
        Message::Close(frame) => UpstreamMessage::Close(frame.map(|frame| {
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: frame.code.into(),
                reason: frame.reason.as_str().to_owned().into(),
            }
        })),
    }
}

fn to_downstream(message: UpstreamMessage) -> Option<Message> {
    Some(match message {
        UpstreamMessage::Text(text) => Message::Text(text.as_str().to_owned().into()),
        UpstreamMessage::Binary(data) => Message::Binary(data),
        UpstreamMessage::Ping(data) => Message::Ping(data),
        UpstreamMessage::Pong(data) => Message::Pong(data),
        UpstreamMessage::Close(frame) => Message::Close(frame.map(|frame| CloseFrame {
            code: frame.code.into(),
            reason: frame.reason.as_str().to_owned().into(),
        })),
        UpstreamMessage::Frame(_) => return None,
    })
}

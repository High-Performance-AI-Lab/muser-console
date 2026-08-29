//! Route table. Only muser-dashboard.html is ever served from ui_dir —
//! there is no generic static handler and therefore no traversal surface.

use axum::body::Body;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HOST, ORIGIN, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use qrcodegen::{QrCode, QrCodeEcc};
use serde::Deserialize;

use crate::auth::{auth_required, error_json, mutation_rejection, read_rejection, valid_bearer};
use crate::history;
use crate::proxy;
use crate::state::{AppState, PAIRING_TTL};
use crate::ws;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/dashboard", get(dashboard))
        .route("/healthz", get(healthz))
        .route("/v1/fleet", get(fleet))
        // History plane. Instance-scoped only: a history query needs a name
        // to join on, and there is no useful fleet-wide default.
        .route(
            "/v1/history/{instance}/series",
            get(history::api::series_list),
        )
        .route("/v1/history/{instance}", get(history::api::query))
        // Root-anchored phase-1 routes: mapped to the default instance so
        // the imported page keeps working before (or without) fleet config.
        .route("/snapshot", get(proxied_buffered))
        .route("/metrics", get(proxied_buffered))
        .route("/telemetry", get(proxied_streaming))
        .route("/v1/chat/completions", post(proxied_streaming))
        .route("/v1/nodes", get(proxied_buffered).post(proxied_buffered))
        .route("/v1/nodes/{name}/progress", get(proxied_streaming))
        .route("/v1/ws-tickets", post(ws::mint))
        .route("/stream", any(ws::stream))
        // Per-instance namespace: same surface with the /i/{name} prefix
        // stripped before forwarding.
        .route("/i/{instance}/snapshot", get(instance_buffered))
        .route("/i/{instance}/metrics", get(instance_buffered))
        .route("/i/{instance}/telemetry", get(instance_streaming))
        .route(
            "/i/{instance}/v1/chat/completions",
            post(instance_streaming),
        )
        .route(
            "/i/{instance}/v1/nodes",
            get(instance_buffered).post(instance_buffered),
        )
        .route(
            "/i/{instance}/v1/nodes/{node}/progress",
            get(instance_streaming),
        )
        .route("/i/{instance}/v1/ws-tickets", post(ws::mint_instance))
        .route("/i/{instance}/stream", any(ws::stream_instance))
        .route("/v1/dashboard/login", post(dashboard_login))
        .route("/v1/dashboard/session", post(dashboard_session))
        .route("/v1/dashboard/pairings", post(pairing_mint))
        .route("/v1/dashboard/pairings/revoke", post(pairing_revoke))
        .route("/v1/dashboard/pairings/redeem", post(pairing_redeem))
        // No fallback: the engine registers none, so unknown routes get
        // axum's default empty-body 404 — exact parity.
        .layer(middleware::from_fn(normalize_request_authority))
        .with_state(state)
}

/// HTTP/2 represents `:authority` as the request URI authority and need not
/// send an HTTP/1 `Host` header. Normalize it before authentication; if a peer
/// supplies both forms, they must agree byte-for-byte.
async fn normalize_request_authority(mut request: Request, next: Next) -> Response {
    if let Err(message) = normalize_authority(&mut request) {
        return error_json(StatusCode::BAD_REQUEST, "invalid_request_error", message);
    }
    next.run(request).await
}

fn normalize_authority(request: &mut Request) -> Result<(), &'static str> {
    let Some(authority) = request.uri().authority().map(|value| value.as_str()) else {
        return Ok(());
    };
    match single_header(request.headers(), HOST.as_str()) {
        Some(host) if host == authority => Ok(()),
        Some(_) => Err("request Host and HTTP/2 authority must match exactly"),
        None if request.headers().contains_key(HOST) => {
            Err("request Host must contain one valid authority")
        }
        None => {
            let value = HeaderValue::from_str(authority)
                .map_err(|_| "request authority must be a valid Host value")?;
            request.headers_mut().insert(HOST, value);
            Ok(())
        }
    }
}

fn single_header<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(first)
}

/// Split `/i/{name}/rest[?query]` into `(name, "/rest[?query]")`. Raw
/// segment, exact string — no percent-decoding, matching the validated
/// `[A-Za-z0-9_-]{1,64}` instance names (a `?` cannot occur before the
/// post-name `/` on any registered `/i/...` route).
pub(crate) fn split_instance(path_and_query: &str) -> Option<(&str, &str)> {
    let rest = path_and_query.strip_prefix("/i/")?;
    let boundary = rest.find('/')?;
    Some((&rest[..boundary], &rest[boundary..]))
}

/// Console-owned namespace error for a name not present in the config.
pub(crate) fn unknown_instance() -> Response {
    error_json(StatusCode::NOT_FOUND, "not_found", "unknown instance")
}

async fn dashboard(State(state): State<AppState>) -> Response {
    let path = state.config().ui_dir.join("muser-dashboard.html");
    let read = tokio::task::spawn_blocking(move || std::fs::read(&path)).await;
    let Ok(Ok(bytes)) = read else {
        return error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "dashboard asset unavailable",
        );
    };
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

async fn healthz() -> Response {
    Json(serde_json::json!({"ok": true})).into_response()
}

/// Console-owned fleet listing: config order, the first instance is the
/// default (the one behind the root-anchored routes). Authorities only —
/// keys never leave the server.
///
/// Each instance carries the agents attached to it. An agent's `state` is
/// only ever what the sampler's last scrape did: `live` if it answered,
/// `unreachable` if it did not, and `unknown` before the first scrape (which
/// includes every console whose history plane is switched off). Nothing here
/// is inferred from configuration — a configured agent is not a running one.
async fn fleet(State(state): State<AppState>, request: Request) -> Response {
    if let Some(response) = read_rejection(&state, request.headers()) {
        return response;
    }
    let instances: Vec<serde_json::Value> = state
        .config()
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| {
            let agents: Vec<serde_json::Value> = state
                .agents_for(&instance.name)
                .map(|agent| {
                    serde_json::json!({
                        "name": agent.name,
                        "kind": agent.kind.as_str(),
                        "state": state.agent_state(&agent.name).as_str(),
                    })
                })
                .collect();
            serde_json::json!({
                "name": instance.name,
                "authority": instance.authority,
                "default": index == 0,
                "agents": agents,
            })
        })
        .collect();
    Json(serde_json::json!({ "instances": instances })).into_response()
}

async fn proxied_buffered(State(state): State<AppState>, request: Request) -> Response {
    proxied(state, request, false).await
}

async fn proxied_streaming(State(state): State<AppState>, request: Request) -> Response {
    proxied(state, request, true).await
}

async fn proxied(state: AppState, request: Request, streaming: bool) -> Response {
    let rejection = if request.method() == Method::POST {
        mutation_rejection(&state, request.headers())
    } else {
        read_rejection(&state, request.headers())
    };
    if let Some(response) = rejection {
        return response;
    }
    let path_and_query = request
        .uri()
        .path_and_query()
        .map_or("/", |value| value.as_str())
        .to_owned();
    proxy::forward(
        &state,
        state.default_instance(),
        request,
        streaming,
        &path_and_query,
    )
    .await
}

async fn instance_buffered(State(state): State<AppState>, request: Request) -> Response {
    instance_proxied(state, request, false).await
}

async fn instance_streaming(State(state): State<AppState>, request: Request) -> Response {
    instance_proxied(state, request, true).await
}

async fn instance_proxied(state: AppState, request: Request, streaming: bool) -> Response {
    // Bearer parity first: an unauthenticated probe cannot enumerate
    // instance names through the 401/404 difference.
    let rejection = if request.method() == Method::POST {
        mutation_rejection(&state, request.headers())
    } else {
        read_rejection(&state, request.headers())
    };
    if let Some(response) = rejection {
        return response;
    }
    let full = request
        .uri()
        .path_and_query()
        .map_or("/", |value| value.as_str())
        .to_owned();
    let Some((name, upstream_path)) = split_instance(&full) else {
        return unknown_instance();
    };
    let Some(instance) = state.instance(name) else {
        return unknown_instance();
    };
    proxy::forward(&state, instance, request, streaming, upstream_path).await
}

async fn dashboard_login(State(state): State<AppState>, request: Request) -> Response {
    if state.config().tls.is_none() {
        return error_json(
            StatusCode::BAD_REQUEST,
            "tls_required",
            "dashboard sessions require HTTPS; use bearer authentication on loopback HTTP",
        );
    }
    let Some(origin) = valid_login_origin(request.headers()) else {
        return error_json(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "dashboard login requires an exact HTTPS Origin and Host match",
        );
    };
    if !valid_bearer(&state.config().access_key, request.headers()) {
        return auth_required();
    }
    let (session, csrf) = match state.mint_session(origin) {
        Ok(values) => values,
        Err(_) => {
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "session entropy unavailable",
            )
        }
    };
    session_response(session, csrf)
}

/// Restore the in-memory CSRF value after a remote dashboard refresh. The
/// HttpOnly session remains the ambient credential; exact same-origin POST
/// is required because this route intentionally cannot require the value it
/// exists to return.
async fn dashboard_session(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if state.config().tls.is_none() {
        return no_store(error_json(
            StatusCode::BAD_REQUEST,
            "tls_required",
            "dashboard sessions require HTTPS; use bearer authentication on loopback HTTP",
        ));
    }
    if valid_login_origin(&headers).is_none() {
        return no_store(error_json(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "dashboard session bootstrap requires an exact HTTPS Origin and Host match",
        ));
    }
    let Some(csrf) = state.session_csrf(&headers) else {
        return no_store(auth_required());
    };
    no_store(Json(serde_json::json!({ "csrf_token": csrf })).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairingRevoke {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairingRedeem {
    token: String,
}

async fn pairing_mint(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !pairing_enabled(&state) {
        return no_store(error_json(
            StatusCode::BAD_REQUEST,
            "pairing_unavailable",
            "device pairing requires a non-loopback HTTPS console",
        ));
    }
    if let Some(response) = mutation_rejection(&state, &headers) {
        return no_store(response);
    }
    let Some(origin) = valid_login_origin(&headers) else {
        return no_store(error_json(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "device pairing requires an exact HTTPS Origin and Host match",
        ));
    };
    let minted = match state.mint_pairing(&headers, origin.clone()) {
        Ok(Some(value)) => value,
        Ok(None) => return no_store(auth_required()),
        Err(_) => {
            return no_store(error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "pairing entropy unavailable",
            ))
        }
    };
    let pairing_url = format!("{origin}/dashboard#pair={}", minted.token);
    let qr = match QrCode::encode_text(&pairing_url, QrCodeEcc::Medium) {
        Ok(value) => value,
        Err(_) => {
            return no_store(error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "pairing address is too long to encode",
            ))
        }
    };
    let size = qr.size();
    let module_count = usize::try_from(size * size).expect("QR dimensions are positive");
    let mut modules = vec![0u8; module_count.div_ceil(8)];
    for y in 0..size {
        for x in 0..size {
            if qr.get_module(x, y) {
                let index = usize::try_from(y * size + x).expect("QR index is positive");
                modules[index / 8] |= 1 << (7 - index % 8);
            }
        }
    }
    no_store(
        Json(serde_json::json!({
            "id": minted.id,
            "pairing_url": pairing_url,
            "expires_in": PAIRING_TTL.as_secs(),
            "qr": {
                "size": size,
                "quiet_zone": 4,
                "modules": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(modules),
            },
        }))
        .into_response(),
    )
}

async fn pairing_revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PairingRevoke>,
) -> Response {
    if !pairing_enabled(&state) {
        return no_store(error_json(
            StatusCode::BAD_REQUEST,
            "pairing_unavailable",
            "device pairing requires a non-loopback HTTPS console",
        ));
    }
    if let Some(response) = mutation_rejection(&state, &headers) {
        return no_store(response);
    }
    if valid_login_origin(&headers).is_none() {
        return no_store(error_json(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "device pairing requires an exact HTTPS Origin and Host match",
        ));
    }
    if body.id.len() > 64 || !state.revoke_pairing(&headers, &body.id) {
        return no_store(error_json(
            StatusCode::BAD_REQUEST,
            "invalid_pairing",
            "pairing is invalid or no longer pending",
        ));
    }
    no_store(Json(serde_json::json!({"revoked": true})).into_response())
}

async fn pairing_redeem(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<PairingRedeem>,
) -> Response {
    if !pairing_enabled(&state) {
        return no_store(error_json(
            StatusCode::BAD_REQUEST,
            "pairing_unavailable",
            "device pairing requires a non-loopback HTTPS console",
        ));
    }
    let Some(origin) = valid_login_origin(&headers) else {
        return no_store(error_json(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "device pairing requires an exact HTTPS Origin and Host match",
        ));
    };
    if !pairing_peer_allowed(peer.ip()) {
        return no_store(error_json(
            StatusCode::FORBIDDEN,
            "pairing_unavailable",
            "device pairing is available only to a directly connected local peer",
        ));
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body.token.as_bytes());
    let Ok(raw_token) = decoded.and_then(|bytes| {
        <[u8; 32]>::try_from(bytes).map_err(|_| base64::DecodeError::InvalidLength(0))
    }) else {
        return invalid_pairing();
    };
    if !state.consume_pairing(&raw_token, &origin) {
        return invalid_pairing();
    }
    let (session, csrf) = match state.mint_session(origin) {
        Ok(values) => values,
        Err(_) => {
            return no_store(error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "session entropy unavailable",
            ))
        }
    };
    session_response(session, csrf)
}

fn pairing_enabled(state: &AppState) -> bool {
    state.config().tls.is_some() && !state.config().listen.ip().is_loopback()
}

fn invalid_pairing() -> Response {
    no_store(error_json(
        StatusCode::BAD_REQUEST,
        "invalid_pairing",
        "pairing is invalid, expired, revoked, or already used",
    ))
}

fn session_response(session: String, csrf: String) -> Response {
    let mut cookie = HeaderValue::from_str(&format!(
        "{}={session}; Path=/; Max-Age=3600; Secure; HttpOnly; SameSite=Strict",
        crate::state::SESSION_COOKIE
    ))
    .expect("base64url session cookie is a valid header value");
    cookie.set_sensitive(true);
    let mut response = Json(serde_json::json!({
        "csrf_token": csrf,
        "expires_in": 3600,
    }))
    .into_response();
    response.headers_mut().insert(SET_COOKIE, cookie);
    no_store(response)
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

fn pairing_peer_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => pairing_v4_allowed(ip),
        IpAddr::V6(ip) => {
            ip.to_ipv4_mapped().is_some_and(pairing_v4_allowed) || pairing_v6_allowed(ip)
        }
    }
}

fn pairing_v4_allowed(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
}

fn pairing_v6_allowed(ip: Ipv6Addr) -> bool {
    ip.is_loopback() || ip.is_unicast_link_local() || (ip.segments()[0] & 0xfe00) == 0xfc00
}

fn valid_login_origin(headers: &axum::http::HeaderMap) -> Option<String> {
    let host = single_header(headers, HOST.as_str())?;
    let authority: axum::http::uri::Authority = host.parse().ok()?;
    if authority.as_str() != host {
        return None;
    }
    let expected = format!("https://{authority}");
    let origin = single_header(headers, ORIGIN.as_str())?;
    (origin.as_bytes() == expected.as_bytes()).then_some(expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http2_authority_normalizes_to_host_and_mismatch_is_rejected() {
        let mut request = Request::builder()
            .uri("https://console.test:8443/v1/dashboard/login")
            .body(Body::empty())
            .expect("request");
        normalize_authority(&mut request).expect("authority must normalize");
        assert_eq!(request.headers().get(HOST).unwrap(), "console.test:8443");

        let mut mismatch = Request::builder()
            .uri("https://console.test:8443/v1/dashboard/login")
            .header(HOST, "other.test:8443")
            .body(Body::empty())
            .expect("request");
        assert!(normalize_authority(&mut mismatch).is_err());
    }

    #[test]
    fn pairing_peer_allowlist_is_local_only_and_unmaps_ipv4() {
        for allowed in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.40.10",
            "169.254.4.5",
            "100.64.0.1",
            "100.127.255.254",
            "::1",
            "fe80::1",
            "fd12:3456::1",
            "::ffff:192.168.40.10",
        ] {
            let ip: IpAddr = allowed.parse().unwrap();
            assert!(pairing_peer_allowed(ip), "{allowed} must be local");
        }
        for public in [
            "0.0.0.0",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.1",
            "224.0.0.1",
            "::",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
        ] {
            let ip: IpAddr = public.parse().unwrap();
            assert!(!pairing_peer_allowed(ip), "{public} must not be local");
        }
    }
}

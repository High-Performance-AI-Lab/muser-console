//! Console -> engine forwarding. The console key never reaches the engine;
//! the instance key never reaches the browser or the logs. SSE routes stream
//! chunk-by-chunk with no timeout — any buffering makes the dashboard cycle
//! stale -> restart.

use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{ACCEPT_ENCODING, AUTHORIZATION, CONNECTION, COOKIE, HOST, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, Version};
use axum::response::Response;
use http_body_util::BodyExt as _;
use tokio::time::timeout;

use crate::auth::error_json;
use crate::config::Instance;
use crate::logging::{error_chain, log};
use crate::state::AppState;

/// Overall timeout for non-streaming proxied requests (head + body).
const BUFFERED_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for the upstream response head on streaming routes. The body is
/// unbounded (SSE ticks forever), but a hung engine must not pin console
/// requests indefinitely before it has even answered.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Forward `request` to `instance`. `path_and_query` is the upstream path —
/// for `/i/{name}/...` routes the caller has already stripped the console's
/// instance prefix; root-anchored routes pass their path unchanged.
pub async fn forward(
    state: &AppState,
    instance: &Instance,
    request: Request,
    streaming: bool,
    path_and_query: &str,
) -> Response {
    let (mut parts, body) = request.into_parts();

    let upstream_uri: Uri = match format!("{}{path_and_query}", instance.base_url).parse() {
        Ok(uri) => uri,
        Err(_) => {
            return error_json(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                &format!("instance '{}' is unreachable", instance.name),
            )
        }
    };

    sanitize_request_headers(&mut parts.headers);
    let Ok(host) = HeaderValue::from_str(&instance.authority) else {
        return unreachable_response(instance);
    };
    parts.headers.insert(HOST, host);
    parts.headers.insert(AUTHORIZATION, instance.bearer.clone());
    parts.uri = upstream_uri;
    // The browser-facing TLS listener negotiates HTTP/2, while the bounded
    // per-instance clients deliberately speak HTTP/1 to match muser's
    // streaming and WebSocket surface. HTTP versions are hop-by-hop: carrying
    // the browser's HTTP/2 marker into the HTTP/1 client makes hyper reject
    // the request with `UserUnsupportedVersion` before any engine sees it.
    normalize_upstream_version(&mut parts.version);
    let upstream_request = Request::from_parts(parts, body);

    if streaming {
        let response = match timeout(
            HEAD_TIMEOUT,
            state.client(instance).request(upstream_request),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                log(&format!(
                    "upstream request to instance '{}' failed: {}",
                    instance.name,
                    error_chain(&error)
                ));
                return unreachable_response(instance);
            }
            Err(_) => {
                log(&format!(
                    "upstream request to instance '{}' timed out before the response head",
                    instance.name
                ));
                return unreachable_response(instance);
            }
        };
        let (mut parts, incoming) = response.into_parts();
        sanitize_response_headers(&mut parts.headers);
        Response::from_parts(parts, Body::new(incoming))
    } else {
        let exchange = async {
            let response = state
                .client(instance)
                .request(upstream_request)
                .await
                .map_err(|error| error.to_string())?;
            let (parts, incoming) = response.into_parts();
            let collected = incoming
                .collect()
                .await
                .map_err(|error| error.to_string())?;
            Ok::<_, String>((parts, collected.to_bytes()))
        };
        let (mut parts, bytes) = match timeout(BUFFERED_TIMEOUT, exchange).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                log(&format!(
                    "upstream request to instance '{}' failed: {error}",
                    instance.name
                ));
                return unreachable_response(instance);
            }
            Err(_) => {
                log(&format!(
                    "upstream request to instance '{}' timed out",
                    instance.name
                ));
                return unreachable_response(instance);
            }
        };
        sanitize_response_headers(&mut parts.headers);
        Response::from_parts(parts, Body::from(bytes))
    }
}

fn unreachable_response(instance: &Instance) -> Response {
    error_json(
        StatusCode::BAD_GATEWAY,
        "upstream_unreachable",
        &format!("instance '{}' is unreachable", instance.name),
    )
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let connection_named: Vec<String> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect();
    for name in connection_named {
        headers.remove(name);
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

fn sanitize_request_headers(headers: &mut HeaderMap) {
    strip_hop_by_hop(headers);
    // The console key and any browser cookies must never reach the engine;
    // the engine never compresses, so keep identity encoding.
    headers.remove(AUTHORIZATION);
    headers.remove(COOKIE);
    headers.remove("x-csrf-token");
    headers.remove(HOST);
    headers.remove(ACCEPT_ENCODING);
}

fn sanitize_response_headers(headers: &mut HeaderMap) {
    strip_hop_by_hop(headers);
    // Defense in depth: the engine only sets cookies on login, which the
    // console terminates.
    headers.remove(SET_COOKIE);
}

fn normalize_upstream_version(version: &mut Version) {
    *version = Version::HTTP_11;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_http2_is_a_separate_hop_from_upstream_http1() {
        let request = Request::builder()
            .version(Version::HTTP_2)
            .uri("https://console.test/snapshot")
            .body(Body::empty())
            .expect("request");
        let (mut parts, _) = request.into_parts();

        normalize_upstream_version(&mut parts.version);

        assert_eq!(parts.version, Version::HTTP_11);
    }
}

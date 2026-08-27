//! Console -> engine forwarding: credential isolation, byte-exact
//! pass-through, status passthrough (including upstream 401), 502 shape.

mod common;

use std::sync::{Arc, Mutex};

use axum::extract::Request;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

#[tokio::test]
async fn snapshot_round_trip_isolates_credentials() {
    let fixture = common::live_engine_snapshot_bytes();
    let seen: Arc<Mutex<Option<HeaderMap>>> = Arc::new(Mutex::new(None));
    let stub_seen = Arc::clone(&seen);
    let stub_body = fixture.clone();
    let stub = Router::new().route(
        "/snapshot",
        get(move |request: Request| {
            let seen = Arc::clone(&stub_seen);
            let body = stub_body.clone();
            async move {
                *seen.lock().expect("stub lock") = Some(request.headers().clone());
                ([(CONTENT_TYPE, "application/json")], body)
            }
        }),
    );
    let upstream = common::spawn_router(stub).await;
    let console = common::spawn_console(upstream).await;
    let client = common::client();

    let (parts, body) = common::request(
        &client,
        "GET",
        console,
        "/snapshot",
        &[
            ("authorization", &common::bearer(common::CONSOLE_KEY)),
            ("cookie", "muser_session=stale-browser-cookie"),
            ("x-csrf-token", "stale-browser-csrf"),
            ("accept-encoding", "gzip, br"),
            ("accept", "application/json"),
        ],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200);
    assert!(
        body.as_ref() == fixture,
        "body must pass through byte-exact"
    );

    let seen = seen.lock().expect("lock").clone().expect("stub was hit");
    let authorization = seen
        .get("authorization")
        .expect("engine bearer must be injected");
    assert!(
        authorization.as_bytes() == common::bearer(common::ENGINE_KEY).as_bytes(),
        "upstream must see the engine instance key"
    );
    assert!(seen.get("cookie").is_none(), "cookie must be stripped");
    assert!(
        seen.get("x-csrf-token").is_none(),
        "x-csrf-token must be stripped"
    );
    assert!(
        seen.get("accept-encoding").is_none(),
        "accept-encoding must be dropped for identity encoding"
    );
    assert_eq!(
        seen.get("accept").expect("accept forwarded"),
        "application/json"
    );
    for value in seen.values() {
        assert!(
            !value
                .as_bytes()
                .windows(common::CONSOLE_KEY.len())
                .any(|window| window == common::CONSOLE_KEY.as_bytes()),
            "console key must never reach the engine"
        );
    }
}

#[tokio::test]
async fn nodes_status_passthrough_including_upstream_401() {
    let seen_content_type: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let stub_seen = Arc::clone(&seen_content_type);
    let stub = Router::new().route(
        "/v1/nodes",
        post(move |request: Request| {
            let seen = Arc::clone(&stub_seen);
            async move {
                *seen.lock().expect("stub lock") = request
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let status = request
                    .headers()
                    .get("x-test-status")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u16>().ok())
                    .expect("test sets x-test-status");
                let marker = format!("{{\"stub\":\"upstream-{status}\"}}");
                Response::builder()
                    .status(StatusCode::from_u16(status).expect("valid status"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(marker))
                    .expect("stub response")
            }
        }),
    );
    let upstream = common::spawn_router(stub).await;
    let console = common::spawn_console(upstream).await;
    let client = common::client();

    for status in [202u16, 409, 401] {
        let (parts, body) = common::request(
            &client,
            "POST",
            console,
            "/v1/nodes",
            &[
                ("authorization", &common::bearer(common::CONSOLE_KEY)),
                ("content-type", "application/json"),
                ("x-test-status", &status.to_string()),
            ],
            b"{}",
        )
        .await;
        assert_eq!(parts.status.as_u16(), status, "status must pass through");
        let expected = format!("{{\"stub\":\"upstream-{status}\"}}");
        assert_eq!(
            body.as_ref(),
            expected.as_bytes(),
            "upstream body must pass through untouched"
        );
        if status == 401 {
            // A 401 from the instance is a distinct engine-credential
            // failure; the console must not replace it with its own 401.
            assert!(
                parts.headers.get("www-authenticate").is_none(),
                "console must not intercept an upstream 401"
            );
        }
    }
    assert_eq!(
        seen_content_type
            .lock()
            .expect("lock")
            .as_deref()
            .expect("stub saw content-type"),
        "application/json",
        "Content-Type must pass through byte-exact (no charset append)"
    );
}

#[tokio::test]
async fn unreachable_instance_becomes_502_envelope() {
    // Nothing listens on port 1; connect fails fast.
    let upstream = "127.0.0.1:1".parse().expect("addr");
    let console = common::spawn_console(upstream).await;
    let client = common::client();

    let (parts, body) = common::request(
        &client,
        "GET",
        console,
        "/snapshot",
        &[("authorization", &common::bearer(common::CONSOLE_KEY))],
        b"",
    )
    .await;
    assert_eq!(parts.status, 502);
    assert_eq!(
        body.as_ref(),
        common::engine_error_body("upstream_unreachable", "instance 'primary' is unreachable")
    );
}

#[tokio::test]
async fn set_cookie_is_stripped_from_upstream_responses() {
    let stub = Router::new().route(
        "/snapshot",
        get(|| async {
            (
                [
                    (CONTENT_TYPE, "application/json"),
                    (axum::http::header::SET_COOKIE, "muser_session=surprise"),
                ],
                "{}",
            )
                .into_response()
        }),
    );
    let upstream = common::spawn_router(stub).await;
    let console = common::spawn_console(upstream).await;
    let client = common::client();

    let (parts, _) = common::request(
        &client,
        "GET",
        console,
        "/snapshot",
        &[("authorization", &common::bearer(common::CONSOLE_KEY))],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200);
    assert!(
        parts.headers.get("set-cookie").is_none(),
        "set-cookie must be stripped"
    );
}

//! Browser -> console bearer parity with the engine's 401 surface.

mod common;

use axum::http::header::CONTENT_TYPE;
use axum::routing::get;
use axum::Router;
use base64::Engine as _;
use console_server::{router, AppState};

fn snapshot_stub(body: Vec<u8>) -> Router {
    Router::new().route(
        "/snapshot",
        get(move || {
            let body = body.clone();
            async move { ([(CONTENT_TYPE, "application/json")], body) }
        }),
    )
}

#[tokio::test]
async fn missing_bearer_gets_engine_exact_401() {
    let upstream = common::spawn_router(snapshot_stub(common::live_engine_snapshot_bytes())).await;
    let console = common::spawn_console(upstream).await;
    let client = common::client();

    let (parts, body) = common::request(&client, "GET", console, "/snapshot", &[], b"").await;
    assert_eq!(parts.status, 401);
    assert_eq!(
        parts
            .headers
            .get("www-authenticate")
            .expect("www-authenticate header"),
        "Bearer"
    );
    assert_eq!(body.as_ref(), common::ENGINE_AUTH_REQUIRED_BODY);
}

#[tokio::test]
async fn wrong_bearer_and_malformed_scheme_rejected() {
    let upstream = common::spawn_router(snapshot_stub(common::live_engine_snapshot_bytes())).await;
    let console = common::spawn_console(upstream).await;
    let client = common::client();

    for authorization in [
        "Bearer definitely-not-the-key",
        "bearer console-access-key-for-tests",
        "Basic console-access-key-for-tests",
        "Bearer",
    ] {
        let (parts, body) = common::request(
            &client,
            "GET",
            console,
            "/snapshot",
            &[("authorization", authorization)],
            b"",
        )
        .await;
        assert_eq!(parts.status, 401, "authorization '{authorization}'");
        assert_eq!(body.as_ref(), common::ENGINE_AUTH_REQUIRED_BODY);
    }
}

#[tokio::test]
async fn plain_http_login_matches_current_engine_literal_bytes() {
    let upstream = common::spawn_router(snapshot_stub(common::live_engine_snapshot_bytes())).await;
    let console = common::spawn_console(upstream).await;
    let client = common::client();
    let (parts, body) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/login",
        &[("content-type", "application/json")],
        br#"{"api_key":"irrelevant"}"#,
    )
    .await;
    assert_eq!(parts.status, 400);
    assert_eq!(body.as_ref(), common::ENGINE_TLS_REQUIRED_BODY);
}

#[tokio::test]
async fn correct_bearer_passes_with_byte_exact_body() {
    let fixture = common::live_engine_snapshot_bytes();
    let upstream = common::spawn_router(snapshot_stub(fixture.clone())).await;
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
    assert_eq!(parts.status, 200);
    assert_eq!(
        parts.headers.get(CONTENT_TYPE).expect("content-type"),
        "application/json"
    );
    assert!(
        body.as_ref() == fixture,
        "body must pass through byte-exact"
    );
}

async fn spawn_session_console(upstream: std::net::SocketAddr) -> std::net::SocketAddr {
    let scratch = common::scratch_dir("session-console");
    let config =
        common::console_config_tls(&scratch, &format!("http://{upstream}"), "0.0.0.0:5959");
    common::spawn_router(router(AppState::new(config))).await
}

struct SessionAuth {
    cookie: String,
    csrf: String,
}

async fn login_session(client: &common::TestClient, console: std::net::SocketAddr) -> SessionAuth {
    let authorization = common::bearer(common::CONSOLE_KEY);
    let (parts, body) = common::request(
        client,
        "POST",
        console,
        "/v1/dashboard/login",
        &[
            ("authorization", &authorization),
            ("host", "console.test"),
            ("origin", "https://console.test"),
        ],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200);
    let cookie = parts
        .headers
        .get("set-cookie")
        .expect("session cookie")
        .to_str()
        .expect("cookie text")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned();
    let value: serde_json::Value = serde_json::from_slice(&body).expect("login JSON");
    let csrf = value
        .get("csrf_token")
        .and_then(serde_json::Value::as_str)
        .expect("CSRF token")
        .to_owned();
    SessionAuth { cookie, csrf }
}

async fn mint_pairing(
    client: &common::TestClient,
    console: std::net::SocketAddr,
    auth: &SessionAuth,
) -> (axum::http::response::Parts, serde_json::Value) {
    let (parts, body) = common::request(
        client,
        "POST",
        console,
        "/v1/dashboard/pairings",
        &[
            ("cookie", &auth.cookie),
            ("host", "console.test"),
            ("origin", "https://console.test"),
            ("x-csrf-token", &auth.csrf),
            ("content-type", "application/json"),
        ],
        b"{}",
    )
    .await;
    let value = serde_json::from_slice(&body).unwrap_or_else(|error| {
        panic!("pairing JSON ({error}): {}", String::from_utf8_lossy(&body))
    });
    (parts, value)
}

#[tokio::test]
async fn tls_login_requires_an_exact_https_origin_host_match() {
    let upstream = common::spawn_router(snapshot_stub(common::live_engine_snapshot_bytes())).await;
    let console = spawn_session_console(upstream).await;
    let client = common::client();
    let authorization = common::bearer(common::CONSOLE_KEY);

    for headers in [
        vec![
            ("authorization", authorization.as_str()),
            ("host", "console.test"),
        ],
        vec![
            ("authorization", authorization.as_str()),
            ("host", "console.test"),
            ("origin", "http://console.test"),
        ],
        vec![
            ("authorization", authorization.as_str()),
            ("host", "console.test"),
            ("origin", "https://other.test"),
        ],
        vec![
            ("authorization", authorization.as_str()),
            ("host", "console.test:443"),
            ("origin", "https://console.test"),
        ],
    ] {
        let (parts, body) = common::request(
            &client,
            "POST",
            console,
            "/v1/dashboard/login",
            &headers,
            b"",
        )
        .await;
        assert_eq!(parts.status, 403, "headers {headers:?}: {body:?}");
        assert!(String::from_utf8_lossy(&body).contains("invalid_origin"));
    }

    let (parts, _) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/login",
        &[
            ("authorization", "Bearer wrong"),
            ("host", "console.test"),
            ("origin", "https://console.test"),
        ],
        b"",
    )
    .await;
    assert_eq!(parts.status, 401);
}

#[tokio::test]
async fn tls_session_is_secure_hour_lived_and_csrf_bound() {
    let fixture = common::live_engine_snapshot_bytes();
    let upstream = common::spawn_router(snapshot_stub(fixture.clone())).await;
    let console = spawn_session_console(upstream).await;
    let client = common::client();
    let authorization = common::bearer(common::CONSOLE_KEY);

    let (parts, body) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/login",
        &[
            ("authorization", &authorization),
            ("host", "console.test"),
            ("origin", "https://console.test"),
        ],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200);
    let set_cookie = parts
        .headers
        .get("set-cookie")
        .expect("session cookie")
        .to_str()
        .expect("cookie text");
    for attribute in [
        "Path=/",
        "Max-Age=3600",
        "Secure",
        "HttpOnly",
        "SameSite=Strict",
    ] {
        assert!(
            set_cookie.contains(attribute),
            "missing {attribute}: {set_cookie}"
        );
    }
    let cookie = set_cookie
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned();
    let login: serde_json::Value = serde_json::from_slice(&body).expect("login JSON");
    assert_eq!(login.get("expires_in").and_then(|v| v.as_u64()), Some(3600));
    let csrf = login
        .get("csrf_token")
        .and_then(|value| value.as_str())
        .expect("CSRF token")
        .to_owned();

    // A non-loopback listener does not accept the reusable access key on API
    // reads. The secure session cookie does, without forwarding it upstream.
    let (parts, _) = common::request(
        &client,
        "GET",
        console,
        "/snapshot",
        &[("authorization", &authorization)],
        b"",
    )
    .await;
    assert_eq!(parts.status, 401);
    let (parts, body) = common::request(
        &client,
        "GET",
        console,
        "/snapshot",
        &[("cookie", &cookie), ("host", "console.test")],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200);
    assert_eq!(body.as_ref(), fixture);

    let (parts, _) = common::request(
        &client,
        "GET",
        console,
        "/snapshot",
        &[("cookie", &cookie), ("host", "other.test")],
        b"",
    )
    .await;
    assert_eq!(
        parts.status, 401,
        "a session is bound to its login authority"
    );

    for csrf_header in [None, Some("wrong-token")] {
        let mut headers = vec![("cookie", cookie.as_str()), ("host", "console.test")];
        if let Some(value) = csrf_header {
            headers.push(("x-csrf-token", value));
        }
        let (parts, body) =
            common::request(&client, "POST", console, "/v1/ws-tickets", &headers, b"").await;
        assert_eq!(parts.status, 403);
        assert!(String::from_utf8_lossy(&body).contains("csrf_required"));
    }

    let (parts, body) = common::request(
        &client,
        "POST",
        console,
        "/v1/ws-tickets",
        &[
            ("cookie", &cookie),
            ("host", "console.test"),
            ("x-csrf-token", &csrf),
        ],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200);
    let ticket: serde_json::Value = serde_json::from_slice(&body).expect("ticket JSON");
    assert!(ticket
        .get("ticket")
        .and_then(|value| value.as_str())
        .is_some());
}

#[tokio::test]
async fn refreshed_tls_session_restores_its_csrf_only_at_exact_origin() {
    let upstream = common::spawn_router(snapshot_stub(common::live_engine_snapshot_bytes())).await;
    let console = spawn_session_console(upstream).await;
    let client = common::client();
    let auth = login_session(&client, console).await;

    for (origin, expected) in [("https://other.test", 403), ("http://console.test", 403)] {
        let (parts, _) = common::request(
            &client,
            "POST",
            console,
            "/v1/dashboard/session",
            &[
                ("cookie", &auth.cookie),
                ("host", "console.test"),
                ("origin", origin),
            ],
            b"",
        )
        .await;
        assert_eq!(parts.status, expected);
    }

    let (parts, body) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/session",
        &[
            ("cookie", &auth.cookie),
            ("host", "console.test"),
            ("origin", "https://console.test"),
        ],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200);
    assert_eq!(parts.headers.get("cache-control").unwrap(), "no-store");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("session JSON");
    assert_eq!(
        value.get("csrf_token").and_then(serde_json::Value::as_str),
        Some(auth.csrf.as_str())
    );
}

#[tokio::test]
async fn pairing_is_exact_origin_local_one_use_and_returns_a_qr_matrix() {
    let upstream = common::spawn_router(snapshot_stub(common::live_engine_snapshot_bytes())).await;
    let console = spawn_session_console(upstream).await;
    let client = common::client();
    let auth = login_session(&client, console).await;

    let (parts, value) = mint_pairing(&client, console, &auth).await;
    assert_eq!(parts.status, 200);
    assert_eq!(parts.headers.get("cache-control").unwrap(), "no-store");
    assert_eq!(parts.headers.get("referrer-policy").unwrap(), "no-referrer");
    assert_eq!(
        value.get("expires_in").and_then(serde_json::Value::as_u64),
        Some(120)
    );
    let url = value
        .get("pairing_url")
        .and_then(serde_json::Value::as_str)
        .expect("pairing URL");
    let token = url
        .strip_prefix("https://console.test/dashboard#pair=")
        .expect("exact-authority fragment URL");
    assert_eq!(token.len(), 43);
    assert!(!String::from_utf8_lossy(&serde_json::to_vec(&value).unwrap()).contains("<svg"));
    let qr = value.get("qr").expect("QR object");
    let size = qr
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .expect("QR size") as usize;
    assert!((21..=177).contains(&size));
    let modules = qr
        .get("modules")
        .and_then(serde_json::Value::as_str)
        .expect("packed QR modules");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(modules)
        .expect("module base64");
    assert_eq!(bytes.len(), (size * size).div_ceil(8));

    // A hostile Origin must not burn the credential.
    let wrong_body = serde_json::to_vec(&serde_json::json!({"token": token})).unwrap();
    let (parts, _) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/pairings/redeem",
        &[
            ("host", "console.test"),
            ("origin", "https://other.test"),
            ("content-type", "application/json"),
        ],
        &wrong_body,
    )
    .await;
    assert_eq!(parts.status, 403);

    let redeem_body = serde_json::to_vec(&serde_json::json!({"token": token})).unwrap();
    let redeem_headers = [
        ("host", "console.test"),
        ("origin", "https://console.test"),
        ("content-type", "application/json"),
    ];
    let (parts, body) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/pairings/redeem",
        &redeem_headers,
        &redeem_body,
    )
    .await;
    assert_eq!(parts.status, 200, "{}", String::from_utf8_lossy(&body));
    let cookie = parts
        .headers
        .get("set-cookie")
        .expect("paired session cookie");
    let cookie = cookie.to_str().unwrap();
    for attribute in ["Max-Age=3600", "Secure", "HttpOnly", "SameSite=Strict"] {
        assert!(cookie.contains(attribute), "missing {attribute}: {cookie}");
    }

    let (parts, body) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/pairings/redeem",
        &redeem_headers,
        &redeem_body,
    )
    .await;
    assert_eq!(parts.status, 400);
    assert!(String::from_utf8_lossy(&body).contains("invalid_pairing"));
}

#[tokio::test]
async fn pairing_requires_csrf_can_be_revoked_and_evicts_oldest_per_session() {
    let upstream = common::spawn_router(snapshot_stub(common::live_engine_snapshot_bytes())).await;
    let console = spawn_session_console(upstream).await;
    let client = common::client();
    let auth = login_session(&client, console).await;

    let (parts, _) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/pairings",
        &[
            ("cookie", &auth.cookie),
            ("host", "console.test"),
            ("origin", "https://console.test"),
        ],
        b"{}",
    )
    .await;
    assert_eq!(parts.status, 403);

    let mut minted = Vec::new();
    for _ in 0..6 {
        let (parts, value) = mint_pairing(&client, console, &auth).await;
        assert_eq!(parts.status, 200);
        minted.push(value);
    }
    let first_token = minted[0]["pairing_url"]
        .as_str()
        .unwrap()
        .rsplit_once("#pair=")
        .unwrap()
        .1;
    let body = serde_json::to_vec(&serde_json::json!({"token": first_token})).unwrap();
    let (parts, _) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/pairings/redeem",
        &[
            ("host", "console.test"),
            ("origin", "https://console.test"),
            ("content-type", "application/json"),
        ],
        &body,
    )
    .await;
    assert_eq!(
        parts.status, 400,
        "the sixth mint evicts the oldest pending code"
    );

    let last = minted.last().unwrap();
    let revoke_body = serde_json::to_vec(&serde_json::json!({"id": last["id"]})).unwrap();
    let (parts, _) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/pairings/revoke",
        &[
            ("cookie", &auth.cookie),
            ("host", "console.test"),
            ("origin", "https://console.test"),
            ("x-csrf-token", &auth.csrf),
            ("content-type", "application/json"),
        ],
        &revoke_body,
    )
    .await;
    assert_eq!(parts.status, 200);
    let last_token = last["pairing_url"]
        .as_str()
        .unwrap()
        .rsplit_once("#pair=")
        .unwrap()
        .1;
    let body = serde_json::to_vec(&serde_json::json!({"token": last_token})).unwrap();
    let (parts, _) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/pairings/redeem",
        &[
            ("host", "console.test"),
            ("origin", "https://console.test"),
            ("content-type", "application/json"),
        ],
        &body,
    )
    .await;
    assert_eq!(parts.status, 400);
}

#[tokio::test]
async fn loopback_http_never_offers_pairing() {
    let upstream = common::spawn_router(snapshot_stub(common::live_engine_snapshot_bytes())).await;
    let console = common::spawn_console(upstream).await;
    let client = common::client();
    let authorization = common::bearer(common::CONSOLE_KEY);
    let (parts, body) = common::request(
        &client,
        "POST",
        console,
        "/v1/dashboard/pairings",
        &[("authorization", &authorization)],
        b"{}",
    )
    .await;
    assert_eq!(parts.status, 400);
    assert!(String::from_utf8_lossy(&body).contains("pairing_unavailable"));
}

//! Phase-3 history plane: the sampler, the rolling store and the query API,
//! exercised against a stub engine that replays literal live `/snapshot`
//! and `/metrics` captures (see `common::replay`).
//!
//! The phase criterion lives here: with the sampler running, the live tile
//! (state plane, `GET /i/{name}/snapshot`) and the newest history point for
//! the same field must be the same number, and that point must be no older
//! than one sample interval. The rest of the file pins the honesty rules —
//! a mock-tagged field stores nothing, an unreported field stores nothing,
//! and a dead instance leaves a gap that ends when it comes back.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Request;
use axum::http::header::CONTENT_TYPE;
use axum::routing::get;
use axum::Router;
use console_server::history::now_ms;
use console_server::history::prom::Exposition;
use console_server::{router, AppState};

/// How long a poll-until-true assertion waits before failing the test.
const DEADLINE: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Stub engine

#[derive(Clone, Default)]
struct Seen {
    /// Authorization header of every scrape, so the test can prove the
    /// sampler injects the instance's engine key server-side.
    authorizations: Arc<Mutex<Vec<String>>>,
}

/// The stub takes the snapshot as bytes so a test can compare the proxied
/// body against exactly what the engine wrote.
fn replay_engine(metrics: String, snapshot: Vec<u8>, seen: Seen) -> Router {
    let metrics_seen = seen.clone();
    Router::new()
        .route(
            "/metrics",
            get(move |request: Request| {
                let body = metrics.clone();
                let seen = metrics_seen.clone();
                async move {
                    record(&seen, &request);
                    (
                        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
                        body,
                    )
                }
            }),
        )
        .route(
            "/snapshot",
            get(move |request: Request| {
                let body = snapshot.clone();
                let seen = seen.clone();
                async move {
                    record(&seen, &request);
                    ([(CONTENT_TYPE, "application/json")], body)
                }
            }),
        )
}

fn snapshot_bytes(snapshot: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(snapshot).expect("serialize replay snapshot")
}

fn record(seen: &Seen, request: &Request) {
    if let Some(value) = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        seen.authorizations
            .lock()
            .expect("stub lock")
            .push(value.to_owned());
    }
}

// ---------------------------------------------------------------------------
// Query helpers

async fn get_json(
    client: &common::TestClient,
    console: SocketAddr,
    path: &str,
) -> (u16, serde_json::Value) {
    let (parts, body) = common::request(
        client,
        "GET",
        console,
        path,
        &[("authorization", &common::bearer(common::CONSOLE_KEY))],
        b"",
    )
    .await;
    let value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (parts.status.as_u16(), value)
}

fn history_path(instance: &str, series: &str, step_s: i64) -> String {
    let now = now_ms();
    format!(
        "/v1/history/{instance}?series={series}&from_ms={}&to_ms={}&step_s={step_s}",
        now - 120_000,
        now + 5_000
    )
}

fn points(body: &serde_json::Value, series: &str) -> Vec<(i64, f64)> {
    body.pointer(&format!("/series/{series}/points"))
        .and_then(serde_json::Value::as_array)
        .map(|points| {
            points
                .iter()
                .map(|point| {
                    (
                        point[0].as_i64().expect("point timestamp is an integer"),
                        point[1].as_f64().expect("point value is a number"),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn newest(body: &serde_json::Value, series: &str) -> Option<(i64, f64)> {
    points(body, series).last().copied()
}

fn field<'a>(body: &'a serde_json::Value, series: &str, name: &str) -> &'a serde_json::Value {
    body.pointer(&format!("/series/{series}/{name}"))
        .unwrap_or(&serde_json::Value::Null)
}

/// Polls `path` until `predicate` holds. The predicate is evaluated right
/// after each response arrives, so freshness checks against `now_ms()`
/// inside it are meaningful.
async fn poll_until(
    client: &common::TestClient,
    console: SocketAddr,
    path_for: impl Fn() -> String,
    label: &str,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let started = std::time::Instant::now();
    let mut last = serde_json::Value::Null;
    while started.elapsed() < DEADLINE {
        let (status, body) = get_json(client, console, &path_for()).await;
        assert_eq!(status, 200, "{label}: {body}");
        if predicate(&body) {
            return body;
        }
        last = body;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {label}; last body: {last}");
}

// ---------------------------------------------------------------------------
// The stub is backed by the literal live capture

#[test]
fn replay_snapshot_is_the_literal_live_engine_capture() {
    let snapshot = common::replay_snapshot(&common::replay());
    assert_eq!(
        snapshot_bytes(&snapshot),
        common::live_engine_snapshot_bytes(),
        "replay must not derive, fill, or reorder the captured engine body"
    );
}

#[test]
fn engine_numbers_survive_the_console_bit_for_bit() {
    // `preserve_order` and `float_roundtrip` together must serialize the
    // parsed live engine body back to the exact bytes that were captured.
    let replay = common::replay();
    let snapshot = common::replay_snapshot(&replay);
    let round_tripped: serde_json::Value =
        serde_json::from_slice(&snapshot_bytes(&snapshot)).expect("snapshot parses");
    assert_eq!(round_tripped, snapshot);
    assert_eq!(
        snapshot_bytes(&snapshot),
        common::live_engine_snapshot_bytes(),
        "live engine ordering and floating-point spellings must survive"
    );

    // The Prometheus side goes through std's float formatting and parsing,
    // both correctly rounded; pin that too.
    let exposition = Exposition::parse(&common::replay_metrics_text(&replay));
    assert_eq!(
        exposition.value("completion_traffic_tok_s_10s", None),
        Some(replay.completion_traffic_tok_s)
    );
}

#[test]
fn parser_reads_the_literal_live_engine_exposition() {
    let replay = common::replay();
    let exposition = Exposition::parse(&common::replay_metrics_text(&replay));

    assert_eq!(
        exposition.value("completion_traffic_tok_s_10s", None),
        Some(replay.completion_traffic_tok_s),
        "the un-prefixed engine wart parses like any other series"
    );
    assert_eq!(
        exposition.value("muser_request_decode_tok_s", None),
        Some(replay.request_decode_tok_s)
    );
    assert_eq!(
        exposition.value("muser_queue_depth", None),
        Some(replay.queue_depth)
    );
    assert_eq!(
        exposition.value("muser_completion_tokens_total", None),
        Some(replay.completion_tokens)
    );
    assert_eq!(
        exposition.value("muser_ttft_milliseconds", Some("0.50")),
        Some(replay.ttft_ms_p50)
    );
    assert_eq!(
        exposition.value("muser_ttft_milliseconds", Some("0.95")),
        Some(replay.ttft_ms_p95)
    );
    assert_eq!(
        exposition.value("muser_itl_milliseconds", Some("0.50")),
        Some(replay.itl_ms)
    );
    // Reported zero is still a measurement: these series are present in the
    // live exposition and must not be confused with an unavailable series.
    assert_eq!(
        exposition.value("muser_itl_milliseconds", Some("0.95")),
        Some(0.0)
    );
    assert_eq!(
        exposition.value("muser_overload_rejections_total", None),
        Some(0.0)
    );
    assert_eq!(
        exposition.value("muser_dflash_acceptance_ratio", None),
        Some(0.0)
    );

    for (phase, seconds) in &replay.phase_seconds {
        let found = exposition
            .samples()
            .iter()
            .find(|sample| {
                sample.metric == "muser_phase_seconds_total"
                    && sample.label("phase") == Some(phase.as_str())
            })
            .unwrap_or_else(|| panic!("phase '{phase}' must parse"));
        assert_eq!(found.value, *seconds, "phase '{phase}'");
    }
}

// ---------------------------------------------------------------------------
// Phase criterion: the chart and the live tile are the same number

#[tokio::test]
async fn live_tile_and_newest_history_point_agree() {
    let replay = common::replay();
    let snapshot = common::replay_snapshot(&replay);
    let served = snapshot_bytes(&snapshot);
    let seen = Seen::default();
    let engine = common::spawn_router(replay_engine(
        common::replay_metrics_text(&replay),
        served.clone(),
        seen.clone(),
    ))
    .await;

    let interval_ms = 200u64;
    let console = common::spawn_console_history(&[("gx", engine)], interval_ms).await;
    let client = common::client();

    // Wait until the store holds a point for both planes' fields that is no
    // older than one sample interval. A live sampler reaches that state; a
    // stalled one never does.
    let body = poll_until(
        &client,
        console.address,
        || history_path("gx", "decode_tok_s,requests_per_s", 1),
        "a decode_tok_s and requests_per_s point within one sample interval",
        |body| {
            let fresh = |series: &str| {
                newest(body, series).is_some_and(|(ts, _)| now_ms() - ts <= interval_ms as i64)
            };
            fresh("decode_tok_s") && fresh("requests_per_s")
        },
    )
    .await;

    // The live tile reads the state plane, untouched by any of this.
    let (parts, live_body) = common::request(
        &client,
        "GET",
        console.address,
        "/i/gx/snapshot",
        &[("authorization", &common::bearer(common::CONSOLE_KEY))],
        b"",
    )
    .await;
    assert_eq!(parts.status, 200);
    assert_eq!(
        live_body.as_ref(),
        served.as_slice(),
        "the state plane still proxies the engine's bytes verbatim"
    );
    let live: serde_json::Value = serde_json::from_slice(&live_body).expect("live snapshot parses");
    // The console must not perturb the engine's numbers on the way through
    // either plane, down to the last bit of the float.
    assert_eq!(live, snapshot, "no value drifts across the round trip");

    let live_decode = live
        .pointer("/_decode/completion_traffic_tok_s_10s")
        .and_then(serde_json::Value::as_f64)
        .expect("live decode traffic");
    let live_requests = live
        .pointer("/wire/requests_per_s")
        .and_then(serde_json::Value::as_f64)
        .expect("live requests/s");

    let (_, stored_decode) = newest(&body, "decode_tok_s").expect("stored decode point");
    let (_, stored_requests) = newest(&body, "requests_per_s").expect("stored requests point");
    assert_eq!(
        stored_decode, live_decode,
        "decode_tok_s must be the same number in both planes, not merely close"
    );
    assert_eq!(
        stored_requests, live_requests,
        "requests_per_s must be the same number in both planes"
    );

    // Provenance travels with the numbers.
    assert_eq!(field(&body, "decode_tok_s", "source"), "metrics");
    assert_eq!(field(&body, "decode_tok_s", "honesty"), "measured");
    assert_eq!(field(&body, "decode_tok_s", "kind"), "gauge");
    assert_eq!(field(&body, "requests_per_s", "source"), "snapshot");
    assert_eq!(field(&body, "requests_per_s", "honesty"), "measured");
    // The whole tag set rides along, so a caller can tell a uniform window
    // from one whose provenance changed part-way through. `honesty` is the
    // sole tag only when there is exactly one.
    assert_eq!(
        field(&body, "decode_tok_s", "honesty_tags"),
        &serde_json::json!(["measured"])
    );

    // The sampler injects the instance's engine key server-side, exactly as
    // the proxy does; the console key never leaves the console.
    let authorizations = seen.authorizations.lock().expect("lock").clone();
    assert!(!authorizations.is_empty(), "the sampler must have scraped");
    let expected = common::bearer(&common::instance_key("gx"));
    for authorization in &authorizations {
        // Compared, never printed: no key material reaches test output.
        assert!(
            authorization == &expected,
            "the sampler must present the instance's own engine key"
        );
        assert!(
            !authorization.contains(common::CONSOLE_KEY),
            "the console key must never reach an engine"
        );
    }
}

// ---------------------------------------------------------------------------
// Honesty: mock-tagged and unreported fields store nothing

#[tokio::test]
async fn mock_tagged_fields_yield_no_stored_series() {
    let replay = common::replay();
    // Structural stimulus: mutate a parsed live capture to tag a second
    // field mock, so the skip path is exercised against the real vocabulary.
    let mut snapshot = common::replay_snapshot(&replay);
    snapshot["_honesty"]["wire"]["ingress_gbps"] = serde_json::json!("mock");

    let engine = common::spawn_router(replay_engine(
        common::replay_metrics_text(&replay),
        snapshot_bytes(&snapshot),
        Seen::default(),
    ))
    .await;
    let console = common::spawn_console_history(&[("gx", engine)], 100).await;
    let client = common::client();

    let body = poll_until(
        &client,
        console.address,
        || history_path("gx", "requests_per_s,wire_ingress_gbps", 1),
        "a requests_per_s point",
        |body| !points(body, "requests_per_s").is_empty(),
    )
    .await;
    assert!(
        points(&body, "wire_ingress_gbps").is_empty(),
        "a mock-tagged field must store nothing at all"
    );
    assert_eq!(
        field(&body, "wire_ingress_gbps", "honesty"),
        &serde_json::Value::Null,
        "a series with no rows makes no honesty claim"
    );
    assert_eq!(
        field(&body, "wire_ingress_gbps", "honesty_tags"),
        &serde_json::json!([]),
        "and holds no tags at all, rather than a default one"
    );

    // Sample for a while longer: a mock field must never sneak in later.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (status, body) = get_json(
        &client,
        console.address,
        &history_path("gx", "wire_ingress_gbps", 1),
    )
    .await;
    assert_eq!(status, 200);
    assert!(points(&body, "wire_ingress_gbps").is_empty());

    // wire.egress_gbps is mock engine-side and has no catalog entry, so it
    // is not even addressable.
    let (status, listing) = get_json(&client, console.address, "/v1/history/gx/series").await;
    assert_eq!(status, 200);
    let names: Vec<String> = listing["series"]
        .as_array()
        .expect("series array")
        .iter()
        .map(|entry| entry["name"].as_str().expect("name").to_owned())
        .collect();
    assert!(
        !names.iter().any(|name| name.contains("egress")),
        "the console must not publish a series for a mock-only field"
    );
    let (status, body) = get_json(
        &client,
        console.address,
        "/v1/history/gx?series=wire_egress_gbps",
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(
        body,
        serde_json::json!({"error": {
            "type": "invalid_request_error",
            "message": "unknown series: 'wire_egress_gbps'"
        }})
    );
}

#[tokio::test]
async fn fields_the_engine_did_not_report_stay_gaps() {
    let replay = common::replay();
    let engine = common::spawn_router(replay_engine(
        common::replay_metrics_text(&replay),
        snapshot_bytes(&common::replay_snapshot(&replay)),
        Seen::default(),
    ))
    .await;
    let console = common::spawn_console_history(&[("gx", engine)], 100).await;
    let client = common::client();

    let reported = "decode_tok_s,itl_ms_p50,itl_ms_p95,overload_rejections_total,\
                    completion_tokens_total,transfers_active,disagg_prefills_total,\
                    remote_fallbacks_total";
    let unreported = "transfer_last_bytes_total,transfer_last_throughput_gbps,\
                      transfer_last_hidden_pct,dflash_accept_rate,dflash_drafted_total";

    let body = poll_until(
        &client,
        console.address,
        || history_path("gx", reported, 1),
        "the reported series to fill",
        |body| {
            reported
                .split(',')
                .all(|name| !points(body, name.trim()).is_empty())
        },
    )
    .await;

    // A measured zero is a measurement: the engine reported an empty
    // transfers array and no disaggregated prefills, so both are stored.
    assert_eq!(newest(&body, "transfers_active").map(|(_, v)| v), Some(0.0));
    assert_eq!(
        newest(&body, "disagg_prefills_total").map(|(_, v)| v),
        Some(0.0)
    );
    assert_eq!(
        newest(&body, "completion_tokens_total").map(|(_, v)| v),
        Some(replay.completion_tokens)
    );
    assert_eq!(
        newest(&body, "itl_ms_p50").map(|(_, v)| v),
        Some(replay.itl_ms)
    );

    // This engine exposes DFlash-shaped counters in both documents, but its
    // snapshot has no `specdec.draft_len`: there is no DFlash runtime. Those
    // zeros are therefore unavailable feature data and must remain gaps.

    let (status, body) =
        get_json(&client, console.address, &history_path("gx", unreported, 1)).await;
    assert_eq!(status, 200);
    for name in unreported.split(',') {
        let name = name.trim();
        assert!(
            points(&body, name).is_empty(),
            "{name}: an unreported field must stay a gap, never a zero"
        );
        assert_eq!(
            field(&body, name, "honesty"),
            &serde_json::Value::Null,
            "{name}: no rows, no honesty claim"
        );
        // The catalog entry is still described, so the UI can say "no
        // history yet" instead of silently dropping the panel.
        assert!(field(&body, name, "kind").is_string());
    }
}

#[tokio::test]
async fn metrics_plane_wins_where_both_planes_carry_a_field() {
    let replay = common::replay();
    // The stub's snapshot carries `_queue_depth` and the exposition carries
    // `muser_queue_depth`. Exactly one series must exist, sourced from
    // /metrics — no duplicate, no disagreement to reconcile later.
    let engine = common::spawn_router(replay_engine(
        common::replay_metrics_text(&replay),
        snapshot_bytes(&common::replay_snapshot(&replay)),
        Seen::default(),
    ))
    .await;
    let console = common::spawn_console_history(&[("gx", engine)], 100).await;
    let client = common::client();

    let body = poll_until(
        &client,
        console.address,
        || history_path("gx", "queue_depth", 1),
        "a queue_depth point",
        |body| !points(body, "queue_depth").is_empty(),
    )
    .await;
    assert_eq!(field(&body, "queue_depth", "source"), "metrics");
    assert_eq!(
        newest(&body, "queue_depth").map(|(_, value)| value),
        Some(replay.queue_depth)
    );

    let (_, listing) = get_json(&client, console.address, "/v1/history/gx/series").await;
    let queue_entries = listing["series"]
        .as_array()
        .expect("series array")
        .iter()
        .filter(|entry| entry["name"] == "queue_depth")
        .count();
    assert_eq!(queue_entries, 1, "one field, one series");
}

// ---------------------------------------------------------------------------
// A dead instance leaves a gap, and the gap ends when it comes back

#[tokio::test]
async fn dead_instance_leaves_a_gap_and_resumes_cleanly() {
    let replay = common::replay();
    let metrics = common::replay_metrics_text(&replay);
    let snapshot = snapshot_bytes(&common::replay_snapshot(&replay));

    let (engine, engine_task) = common::spawn_router_stoppable(replay_engine(
        metrics.clone(),
        snapshot.clone(),
        Seen::default(),
    ))
    .await;
    let console = common::spawn_console_history(&[("gx", engine)], 100).await;
    let client = common::client();

    let path = || history_path("gx", "decode_tok_s", 1);
    poll_until(
        &client,
        console.address,
        path,
        "the first decode_tok_s points",
        |body| points(body, "decode_tok_s").len() >= 3,
    )
    .await;

    // Kill the engine outright: listener and in-flight connections drop.
    engine_task.abort();
    let mut dead = false;
    for _ in 0..100 {
        let (status, _) = get_json(&client, console.address, "/i/gx/snapshot").await;
        if status == 502 {
            dead = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(dead, "the stub engine must actually be down");

    // Let several sample intervals elapse against a dead instance, then
    // confirm the store stopped growing: the sampler records nothing rather
    // than repeating the last value it saw.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (_, body) = get_json(&client, console.address, &path()).await;
    let frozen = points(&body, "decode_tok_s").len();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (_, body) = get_json(&client, console.address, &path()).await;
    assert_eq!(
        points(&body, "decode_tok_s").len(),
        frozen,
        "a dead instance must add no rows at all"
    );
    let gap_end = newest(&body, "decode_tok_s")
        .expect("points from before the death")
        .0;
    assert!(
        now_ms() - gap_end >= 500,
        "the newest point must be visibly stale while the instance is down"
    );

    // Bring the same authority back up.
    let _revived =
        common::spawn_router_at(engine, replay_engine(metrics, snapshot, Seen::default())).await;
    let body = poll_until(
        &client,
        console.address,
        path,
        "sampling to resume after the instance returns",
        move |body| points(body, "decode_tok_s").len() > frozen,
    )
    .await;

    // The gap is still a gap: resuming appends new points, it does not
    // backfill the time the instance was down.
    let stored = points(&body, "decode_tok_s");
    let largest_gap = stored
        .windows(2)
        .map(|pair| pair[1].0 - pair[0].0)
        .max()
        .expect("at least two points");
    assert!(
        largest_gap >= 500,
        "the outage must remain visible as a gap between points, got {largest_gap} ms"
    );
    assert!(
        stored
            .iter()
            .all(|(_, value)| *value == replay.completion_traffic_tok_s),
        "every stored point is a value the engine actually reported"
    );
}

// ---------------------------------------------------------------------------
// Query API surface

#[tokio::test]
async fn series_listing_is_the_static_catalog() {
    let dead: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let console = common::spawn_console_history(&[("gx", dead)], 60_000).await;
    let client = common::client();

    let (status, body) = get_json(&client, console.address, "/v1/history/gx/series").await;
    assert_eq!(status, 200);
    let entries = body["series"].as_array().expect("series array");
    assert!(!entries.is_empty());
    for entry in entries {
        let name = entry["name"].as_str().expect("name");
        assert!(
            matches!(entry["kind"].as_str(), Some("gauge") | Some("counter")),
            "{name} kind"
        );
        let source = entry["source"].as_str().expect("source");
        assert!(
            matches!(source, "metrics" | "snapshot" | "agent"),
            "{name} source"
        );
        assert!(entry["unit"].is_string(), "{name} unit");
        match source {
            // The exposition carries measured series only, so there is no
            // honesty path to read — null, not an empty string. An agent
            // series has no engine sidecar behind it at all, for the same
            // reason: the engine never saw the number.
            "metrics" | "agent" => assert!(entry["honesty_path"].is_null(), "{name} honesty_path"),
            _ => assert!(entry["honesty_path"].is_string(), "{name} honesty_path"),
        }
    }

    let decode = entries
        .iter()
        .find(|entry| entry["name"] == "decode_tok_s")
        .expect("decode_tok_s listed");
    assert_eq!(
        decode,
        &serde_json::json!({
            "name": "decode_tok_s",
            "kind": "gauge",
            "source": "metrics",
            "honesty_path": null,
            "unit": "tok/s"
        })
    );
    let requests = entries
        .iter()
        .find(|entry| entry["name"] == "requests_per_s")
        .expect("requests_per_s listed");
    assert_eq!(requests["honesty_path"], "wire.requests_per_s");
}

#[tokio::test]
async fn query_rejections_use_the_console_envelopes() {
    let dead: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let console = common::spawn_console_history(&[("gx", dead)], 60_000).await;
    let client = common::client();

    // Bearer first: an unauthenticated probe cannot tell a real instance
    // name from a made-up one.
    for path in [
        "/v1/history/gx",
        "/v1/history/gx/series",
        "/v1/history/nope",
        "/v1/history/nope/series",
    ] {
        let (parts, body) = common::request(&client, "GET", console.address, path, &[], b"").await;
        assert_eq!(parts.status, 401, "{path}");
        assert_eq!(
            parts.headers.get("www-authenticate").expect("header"),
            "Bearer"
        );
        assert_eq!(
            body.as_ref(),
            common::engine_error_body(
                "authentication_required",
                "a valid bearer API key is required"
            ),
            "{path}"
        );
    }

    // Unknown instance: the console's existing 404 envelope.
    for path in ["/v1/history/nope", "/v1/history/nope/series"] {
        let (status, body) = get_json(&client, console.address, path).await;
        assert_eq!(status, 404, "{path}");
        assert_eq!(
            body,
            serde_json::json!({"error": {"type": "not_found", "message": "unknown instance"}}),
            "{path}"
        );
    }

    for (query, expected) in [
        (
            "series=decode_tok_s,not_a_series",
            "unknown series: 'not_a_series'",
        ),
        (
            "from_ms=100&to_ms=100",
            "to_ms must be greater than from_ms",
        ),
        (
            "from_ms=200&to_ms=100",
            "to_ms must be greater than from_ms",
        ),
        ("step_s=0", "step_s must be at least 1"),
        ("from_ms=x", "query parameter 'from_ms' must be an integer"),
        ("surprise=1", "unknown query parameter 'surprise'"),
        (
            "from_ms=0&to_ms=86400000&step_s=1",
            "range would return 86400 points per series at step_s=1; \
             raise step_s (limit 20000)",
        ),
    ] {
        let (status, body) =
            get_json(&client, console.address, &format!("/v1/history/gx?{query}")).await;
        assert_eq!(status, 400, "{query}");
        assert_eq!(
            body,
            serde_json::json!({"error": {
                "type": "invalid_request_error",
                "message": expected
            }}),
            "{query}"
        );
    }

    // The same wide range is served once the caller widens the step.
    let (status, _) = get_json(
        &client,
        console.address,
        "/v1/history/gx?from_ms=0&to_ms=86400000&step_s=60",
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_console_without_the_history_plane_says_so() {
    let dead: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let scratch = common::scratch_dir("history-off");
    let config = common::console_config_fleet(&scratch, &[("gx", format!("http://{dead}"))]);
    let console = common::spawn_router(router(AppState::new(config))).await;
    let client = common::client();

    for path in ["/v1/history/gx", "/v1/history/gx/series"] {
        let (status, body) = get_json(&client, console, path).await;
        assert_eq!(status, 503, "{path}");
        assert_eq!(
            body,
            serde_json::json!({"error": {
                "type": "history_unavailable",
                "message": "the history plane is disabled on this console"
            }}),
            "{path}"
        );
    }
    // Auth and instance resolution still come first.
    let (parts, _) = common::request(&client, "GET", console, "/v1/history/gx", &[], b"").await;
    assert_eq!(parts.status, 401);
    let (status, _) = get_json(&client, console, "/v1/history/nope").await;
    assert_eq!(status, 404);
}

// ---------------------------------------------------------------------------
// Store behaviour end to end

#[tokio::test]
async fn instances_do_not_share_history() {
    let replay = common::replay();
    let engine = common::spawn_router(replay_engine(
        common::replay_metrics_text(&replay),
        snapshot_bytes(&common::replay_snapshot(&replay)),
        Seen::default(),
    ))
    .await;
    // 'mac' points at a port nothing listens on: it must accumulate nothing
    // while 'gx' fills, and it must not inherit gx's rows.
    let dead: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let console = common::spawn_console_history(&[("gx", engine), ("mac", dead)], 100).await;
    let client = common::client();

    poll_until(
        &client,
        console.address,
        || history_path("gx", "decode_tok_s", 1),
        "gx to fill",
        |body| points(body, "decode_tok_s").len() >= 3,
    )
    .await;
    let (status, body) = get_json(
        &client,
        console.address,
        &history_path("mac", "decode_tok_s", 1),
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        points(&body, "decode_tok_s").is_empty(),
        "an unreachable instance's history stays empty, never borrowed"
    );
}

#[tokio::test]
async fn maintenance_runs_at_startup_and_leaves_fresh_rows_alone() {
    let replay = common::replay();
    let engine = common::spawn_router(replay_engine(
        common::replay_metrics_text(&replay),
        snapshot_bytes(&common::replay_snapshot(&replay)),
        Seen::default(),
    ))
    .await;
    let console = common::spawn_console_history(&[("gx", engine)], 100).await;
    let client = common::client();

    let body = poll_until(
        &client,
        console.address,
        || history_path("gx", "decode_tok_s", 1),
        "points to store",
        |body| points(body, "decode_tok_s").len() >= 3,
    )
    .await;
    let before = points(&body, "decode_tok_s");

    // A pass over a store whose rows are all inside the 24 h raw window
    // must do nothing: no collapse, no expiry, no rewritten values.
    let store = console.state.history().expect("history store");
    let report = store.maintain_at(now_ms()).await.expect("maintenance");
    assert_eq!(report.buckets_rewritten, 0);
    assert_eq!(report.rows_collapsed, 0);
    assert_eq!(report.rows_expired, 0);

    let (_, body) = get_json(
        &client,
        console.address,
        &history_path("gx", "decode_tok_s", 1),
    )
    .await;
    let after = points(&body, "decode_tok_s");
    assert!(
        after.starts_with(&before),
        "maintenance must not disturb rows inside the raw window"
    );
    assert!(
        console.db_path.is_file(),
        "the store file must exist on disk"
    );
}

#[tokio::test]
async fn wide_steps_bucket_without_inventing_points() {
    let replay = common::replay();
    let engine = common::spawn_router(replay_engine(
        common::replay_metrics_text(&replay),
        snapshot_bytes(&common::replay_snapshot(&replay)),
        Seen::default(),
    ))
    .await;
    let console = common::spawn_console_history(&[("gx", engine)], 100).await;
    let client = common::client();

    poll_until(
        &client,
        console.address,
        || history_path("gx", "decode_tok_s", 1),
        "several points",
        |body| points(body, "decode_tok_s").len() >= 6,
    )
    .await;

    let (status, body) = get_json(
        &client,
        console.address,
        &history_path("gx", "decode_tok_s,completion_tokens_total", 60),
    )
    .await;
    assert_eq!(status, 200);
    let bucketed = points(&body, "decode_tok_s");
    assert!(!bucketed.is_empty());
    assert!(
        bucketed.len() <= 3,
        "a 125 s window at step_s=60 cannot yield {} points",
        bucketed.len()
    );
    for (ts, value) in &bucketed {
        assert_eq!(ts % 60_000, 0, "buckets are epoch-anchored");
        // Every sample carried the same reported value, so its mean is that
        // value — a bucket never conjures a number outside its inputs.
        assert_eq!(*value, replay.completion_traffic_tok_s);
    }
    for (_, value) in points(&body, "completion_tokens_total") {
        assert_eq!(
            value, replay.completion_tokens,
            "counters keep a real value"
        );
    }
}

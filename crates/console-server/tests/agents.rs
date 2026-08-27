//! Phase-4 agent wiring: the console scrapes sidecar exporters, stores what
//! they published as `agent`/`agent-measured`, and reports each agent's
//! reachability on `/v1/fleet`.
//!
//! The stub exporter below serves exposition text written here in the test.
//! That text is plumbing, not telemetry: the values are ordinals picked to be
//! distinguishable from each other, none of them is a measurement of
//! anything, and none of them is asserted as one. What is asserted is shape —
//! which series exist, which device they name, how they are tagged, and what
//! happens when the exporter is unreachable or says nothing.
//!
//! The engine side of these tests replays real captured measurements exactly
//! as tests/history.rs does, so "the agent did not disturb the engine's
//! series" is checked against numbers the engine actually reported.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Request;
use axum::http::header::CONTENT_TYPE;
use axum::routing::get;
use axum::Router;
use console_server::history::now_ms;
use console_server::{router, AppState};

const DEADLINE: Duration = Duration::from_secs(20);
type HeaderLine = Vec<(String, String)>;
type HeaderLog = Arc<Mutex<Vec<HeaderLine>>>;

// ---------------------------------------------------------------------------
// Stubs

/// A stub exporter: one `/metrics` route serving fixed exposition text, plus
/// the headers of every scrape so a test can prove the console sent no
/// credential to it.
#[derive(Clone, Default)]
struct SeenHeaders {
    lines: HeaderLog,
}

fn stub_exporter(exposition: String, seen: SeenHeaders) -> Router {
    Router::new().route(
        "/metrics",
        get(move |request: Request| {
            let body = exposition.clone();
            let seen = seen.clone();
            async move {
                let headers: Vec<(String, String)> = request
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_owned(),
                            String::from_utf8_lossy(value.as_bytes()).into_owned(),
                        )
                    })
                    .collect();
                seen.lines.lock().expect("stub lock").push(headers);
                (
                    [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
                    body,
                )
            }
        }),
    )
}

/// The engine stub from the history tests, replaying real captured values.
fn replay_engine(metrics: String, snapshot: Vec<u8>) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(move || {
                let body = metrics.clone();
                async move {
                    (
                        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
                        body,
                    )
                }
            }),
        )
        .route(
            "/snapshot",
            get(move || {
                let body = snapshot.clone();
                async move { ([(CONTENT_TYPE, "application/json")], body) }
            }),
        )
}

async fn spawn_engine() -> (SocketAddr, common::Replay) {
    let replay = common::replay();
    let address = common::spawn_router(replay_engine(
        common::replay_metrics_text(&replay),
        serde_json::to_vec(&common::replay_snapshot(&replay)).expect("serialize snapshot"),
    ))
    .await;
    (address, replay)
}

/// A two-GPU exposition in the shape agents/gx10-exporter serves. Structural
/// values only; device 1 deliberately publishes no temperature so the
/// per-field omission rule is exercised end to end.
fn gx10_exposition() -> String {
    "\
# HELP muser_agent_up 1 when the source answered this scrape
# TYPE muser_agent_up gauge
muser_agent_up{agent=\"gx10\"} 1
# HELP muser_agent_scrape_duration_seconds how long the source took
# TYPE muser_agent_scrape_duration_seconds gauge
muser_agent_scrape_duration_seconds{agent=\"gx10\"} 0.001
# HELP muser_gpu_utilization_ratio NVML percent, divided by 100
# TYPE muser_gpu_utilization_ratio gauge
muser_gpu_utilization_ratio{gpu=\"0\",uuid=\"GPU-aaa\",name=\"Dev \\\"A\\\"\"} 0.25
muser_gpu_utilization_ratio{gpu=\"1\",uuid=\"GPU-bbb\",name=\"Dev B\"} 0.75
# TYPE muser_gpu_power_watts gauge
muser_gpu_power_watts{gpu=\"0\",uuid=\"GPU-aaa\",name=\"Dev \\\"A\\\"\"} 11
muser_gpu_power_watts{gpu=\"1\",uuid=\"GPU-bbb\",name=\"Dev B\"} 22
# TYPE muser_gpu_temperature_celsius gauge
muser_gpu_temperature_celsius{gpu=\"0\",uuid=\"GPU-aaa\",name=\"Dev \\\"A\\\"\"} 33
# TYPE muser_gpu_memory_used_bytes gauge
muser_gpu_memory_used_bytes{gpu=\"0\",uuid=\"GPU-aaa\",name=\"Dev \\\"A\\\"\"} 1024
# TYPE muser_gpu_memory_total_bytes gauge
muser_gpu_memory_total_bytes{gpu=\"0\",uuid=\"GPU-aaa\",name=\"Dev \\\"A\\\"\"} 4096
"
    .to_owned()
}

/// The mac exporter's shape: host power, no devices.
fn mac_exposition() -> String {
    "\
# TYPE muser_agent_up gauge
muser_agent_up{agent=\"mac\"} 1
# TYPE muser_host_package_power_watts gauge
muser_host_package_power_watts{host=\"studio\"} 7
"
    .to_owned()
}

// ---------------------------------------------------------------------------
// Query helpers (same shape as tests/history.rs)

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

fn history_path(instance: &str, series: &str) -> String {
    let now = now_ms();
    format!(
        "/v1/history/{instance}?series={series}&from_ms={}&to_ms={}&step_s=1",
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

/// The agents array `/v1/fleet` reports for one instance.
async fn fleet_agents(
    client: &common::TestClient,
    console: SocketAddr,
    instance: &str,
) -> Vec<serde_json::Value> {
    let (status, body) = get_json(client, console, "/v1/fleet").await;
    assert_eq!(status, 200, "{body}");
    body["instances"]
        .as_array()
        .expect("instances array")
        .iter()
        .find(|entry| entry["name"] == instance)
        .and_then(|entry| entry["agents"].as_array())
        .cloned()
        .unwrap_or_default()
}

async fn poll_agent_state(
    client: &common::TestClient,
    console: SocketAddr,
    instance: &str,
    agent: &str,
    wanted: &str,
) {
    let started = std::time::Instant::now();
    let mut last = String::new();
    while started.elapsed() < DEADLINE {
        let agents = fleet_agents(client, console, instance).await;
        if let Some(entry) = agents.iter().find(|entry| entry["name"] == agent) {
            last = entry["state"].to_string();
            if entry["state"] == wanted {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("agent '{agent}' never reached state '{wanted}'; last was {last}");
}

// ---------------------------------------------------------------------------
// Storage and provenance

#[tokio::test]
async fn agent_samples_are_stored_as_agent_measured_under_their_instance() {
    let (engine, replay) = spawn_engine().await;
    let seen = SeenHeaders::default();
    let exporter = common::spawn_router(stub_exporter(gx10_exposition(), seen.clone())).await;
    let console = common::spawn_console_agents(
        &[("gx", engine)],
        &[("gx-gpu", exporter, "gx", "gx10")],
        100,
    )
    .await;
    let client = common::client();

    let wanted = "gpu0_utilization_ratio,gpu0_power_watts,gpu0_temperature_celsius,\
                  gpu0_memory_used_bytes,gpu0_memory_total_bytes,gpu1_utilization_ratio,\
                  gpu1_temperature_celsius,gpu2_utilization_ratio,decode_tok_s";
    let body = poll_until(
        &client,
        console.address,
        || history_path("gx", wanted),
        "the exporter's series to fill",
        |body| {
            !points(body, "gpu0_utilization_ratio").is_empty()
                && !points(body, "gpu1_utilization_ratio").is_empty()
                && !points(body, "decode_tok_s").is_empty()
        },
    )
    .await;

    // Provenance: agent series are tagged as the sidecar's, never the
    // engine's, and they are stored under the instance the hardware serves.
    assert_eq!(field(&body, "gpu0_utilization_ratio", "source"), "agent");
    assert_eq!(
        field(&body, "gpu0_utilization_ratio", "honesty"),
        "agent-measured",
        "an agent number must not be able to present itself as engine-measured"
    );
    assert_eq!(field(&body, "gpu0_utilization_ratio", "kind"), "gauge");

    // Each device's series reads its own labels, and every published field
    // lands.
    for series in [
        "gpu0_utilization_ratio",
        "gpu0_power_watts",
        "gpu0_temperature_celsius",
        "gpu0_memory_used_bytes",
        "gpu0_memory_total_bytes",
        "gpu1_utilization_ratio",
    ] {
        assert!(
            !points(&body, series).is_empty(),
            "{series}: the exporter published it, so the store must hold it"
        );
        assert_eq!(
            field(&body, series, "honesty"),
            "agent-measured",
            "{series}"
        );
    }
    assert_ne!(
        newest(&body, "gpu0_utilization_ratio").map(|(_, value)| value),
        newest(&body, "gpu1_utilization_ratio").map(|(_, value)| value),
        "the two devices must not collide on one row"
    );

    // A field the exporter omitted for one device is a gap for that field
    // alone — the rest of the device still publishes.
    assert!(
        points(&body, "gpu1_temperature_celsius").is_empty(),
        "an omitted probe stores nothing, never a zero or a neighbour's value"
    );
    assert_eq!(
        field(&body, "gpu1_temperature_celsius", "honesty"),
        &serde_json::Value::Null,
        "no rows, no honesty claim"
    );
    assert!(
        points(&body, "gpu2_utilization_ratio").is_empty(),
        "a device the exporter never mentioned has no series at all"
    );

    // The engine's own series are untouched by any of this.
    assert_eq!(field(&body, "decode_tok_s", "source"), "metrics");
    assert_eq!(field(&body, "decode_tok_s", "honesty"), "measured");
    assert_eq!(
        newest(&body, "decode_tok_s").map(|(_, value)| value),
        Some(replay.completion_traffic_tok_s)
    );

    // The console never offers a credential to an exporter: not an engine
    // key, not its own access key, not a cookie.
    let scrapes = seen.lines.lock().expect("lock").clone();
    assert!(
        !scrapes.is_empty(),
        "the sampler must have scraped the agent"
    );
    for headers in &scrapes {
        for (name, value) in headers {
            assert!(
                !matches!(name.as_str(), "authorization" | "cookie" | "x-csrf-token"),
                "the console must send no credential to an exporter (saw '{name}')"
            );
            assert!(
                !value.contains(common::CONSOLE_KEY) && !value.contains("engine-instance-key"),
                "no key material may appear in any header sent to an exporter"
            );
        }
    }
}

#[tokio::test]
async fn a_host_power_agent_stores_only_what_its_sampler_reported() {
    let (engine, _replay) = spawn_engine().await;
    let exporter =
        common::spawn_router(stub_exporter(mac_exposition(), SeenHeaders::default())).await;
    let console = common::spawn_console_agents(
        &[("mac", engine)],
        &[("mac-power", exporter, "mac", "mac")],
        100,
    )
    .await;
    let client = common::client();

    let series = "host_package_power_watts,host_cpu_power_watts,host_gpu_power_watts,\
                  gpu0_utilization_ratio";
    let body = poll_until(
        &client,
        console.address,
        || history_path("mac", series),
        "host package power to fill",
        |body| !points(body, "host_package_power_watts").is_empty(),
    )
    .await;
    assert_eq!(
        field(&body, "host_package_power_watts", "honesty"),
        "agent-measured"
    );
    for absent in [
        "host_cpu_power_watts",
        "host_gpu_power_watts",
        "gpu0_utilization_ratio",
    ] {
        assert!(
            points(&body, absent).is_empty(),
            "{absent}: a reading this exporter never published stays a gap"
        );
    }
}

// ---------------------------------------------------------------------------
// Unreachable agents

#[tokio::test]
async fn an_unreachable_agent_leaves_a_gap_without_disturbing_its_instance() {
    let (engine, replay) = spawn_engine().await;
    // Nothing listens here; the agent can never answer.
    let dead: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let console =
        common::spawn_console_agents(&[("gx", engine)], &[("gx-gpu", dead, "gx", "gx10")], 100)
            .await;
    let client = common::client();

    let series = "decode_tok_s,requests_per_s,gpu0_utilization_ratio";
    let body = poll_until(
        &client,
        console.address,
        || history_path("gx", series),
        "the engine's own series to keep filling",
        |body| points(body, "decode_tok_s").len() >= 3,
    )
    .await;

    // The engine's planes are unaffected by the dead sidecar next to them.
    assert_eq!(
        newest(&body, "decode_tok_s").map(|(_, value)| value),
        Some(replay.completion_traffic_tok_s)
    );
    assert!(!points(&body, "requests_per_s").is_empty());

    assert!(
        points(&body, "gpu0_utilization_ratio").is_empty(),
        "an unreachable agent records nothing at all"
    );
    assert_eq!(
        field(&body, "gpu0_utilization_ratio", "honesty"),
        &serde_json::Value::Null,
        "a series with no rows makes no honesty claim"
    );

    // Keep sampling: an agent that is down must never start producing rows.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (status, body) = get_json(&client, console.address, &history_path("gx", series)).await;
    assert_eq!(status, 200);
    assert!(points(&body, "gpu0_utilization_ratio").is_empty());

    poll_agent_state(&client, console.address, "gx", "gx-gpu", "unreachable").await;
}

// ---------------------------------------------------------------------------
// /v1/fleet agent states

#[tokio::test]
async fn fleet_reports_agent_state_from_the_last_scrape_only() {
    let (engine, _replay) = spawn_engine().await;
    let live_exporter =
        common::spawn_router(stub_exporter(gx10_exposition(), SeenHeaders::default())).await;
    let dead: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let console = common::spawn_console_agents(
        &[("gx", engine), ("mac", dead)],
        &[
            ("gx-gpu", live_exporter, "gx", "gx10"),
            ("gx-host", dead, "gx", "mac"),
            ("mac-power", dead, "mac", "mac"),
        ],
        100,
    )
    .await;
    let client = common::client();

    poll_agent_state(&client, console.address, "gx", "gx-gpu", "live").await;
    poll_agent_state(&client, console.address, "gx", "gx-host", "unreachable").await;
    poll_agent_state(&client, console.address, "mac", "mac-power", "unreachable").await;

    // Agents are listed under the instance they name, in config order, with
    // their kind — and a fleet listing still carries no key material.
    let gx = fleet_agents(&client, console.address, "gx").await;
    assert_eq!(gx.len(), 2);
    assert_eq!(gx[0]["name"], "gx-gpu");
    assert_eq!(gx[0]["kind"], "gx10");
    assert_eq!(gx[1]["name"], "gx-host");
    let mac = fleet_agents(&client, console.address, "mac").await;
    assert_eq!(mac.len(), 1);
    assert_eq!(mac[0]["kind"], "mac");

    let (_, body) = get_json(&client, console.address, "/v1/fleet").await;
    let text = body.to_string();
    assert!(
        !text.contains(common::CONSOLE_KEY) && !text.contains("engine-instance-key"),
        "the fleet listing must carry no key material"
    );
}

#[tokio::test]
async fn an_agent_that_has_never_been_scraped_is_unknown_not_down() {
    // With the history plane off, no sampler runs, so no scrape has happened.
    // The console must say so rather than guess at the exporter's health.
    let dead: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let scratch = common::scratch_dir("agents-unknown");
    let config = common::console_config_agents(
        &scratch,
        &[("gx", format!("http://{dead}"))],
        &[("gx-gpu", format!("http://{dead}"), "gx", "gx10")],
        60_000,
    );
    let console = common::spawn_router(router(AppState::new(config))).await;
    let client = common::client();

    let agents = fleet_agents(&client, console, "gx").await;
    assert_eq!(
        agents,
        vec![serde_json::json!({
            "name": "gx-gpu", "kind": "gx10", "state": "unknown"
        })],
        "a configured agent is not a scraped one"
    );
}

// ---------------------------------------------------------------------------
// Catalog surface

#[tokio::test]
async fn agent_series_are_addressable_through_the_history_api() {
    let dead: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let console =
        common::spawn_console_agents(&[("gx", dead)], &[("gx-gpu", dead, "gx", "gx10")], 60_000)
            .await;
    let client = common::client();

    let (status, body) = get_json(&client, console.address, "/v1/history/gx/series").await;
    assert_eq!(status, 200);
    let entries = body["series"].as_array().expect("series array");
    let util = entries
        .iter()
        .find(|entry| entry["name"] == "gpu0_utilization_ratio")
        .expect("gpu0_utilization_ratio listed");
    assert_eq!(
        util,
        &serde_json::json!({
            "name": "gpu0_utilization_ratio",
            "kind": "gauge",
            "source": "agent",
            // No engine sidecar stands behind an agent series.
            "honesty_path": null,
            "unit": "ratio"
        })
    );
    let power = entries
        .iter()
        .find(|entry| entry["name"] == "host_package_power_watts")
        .expect("host_package_power_watts listed");
    assert_eq!(power["source"], "agent");
    assert_eq!(power["unit"], "W");

    // The dashboard asks for every series it draws in one request, agent
    // series included, so that the charts and the node panel always describe
    // the same instant. That whole list has to be addressable in one URL.
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry["name"].as_str().expect("name"))
        .collect();
    let (status, body) = get_json(
        &client,
        console.address,
        &format!("/v1/history/gx?series={}&step_s=60", names.join(",")),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    for name in &names {
        assert!(
            body.pointer(&format!("/series/{name}")).is_some(),
            "{name} must come back described even with no rows"
        );
    }

    // A device past the catalog's cap is not addressable, and says so.
    let (status, body) = get_json(
        &client,
        console.address,
        "/v1/history/gx?series=gpu8_utilization_ratio",
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(
        body,
        serde_json::json!({"error": {
            "type": "invalid_request_error",
            "message": "unknown series: 'gpu8_utilization_ratio'"
        }})
    );
}

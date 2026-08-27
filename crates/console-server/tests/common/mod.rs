#![allow(dead_code)]
//! Shared integration-test plumbing. Product-plane stub payloads are literal
//! `/snapshot` and `/metrics` bytes captured from the live Phase-1 engine.
//! Tests that mutate a parsed copy do so only as explicitly labelled
//! structural stimuli, never as claimed measurements.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Bytes;
use axum::http::response::Parts;
use axum::Router;
use console_server::{router, AppState, Config, HistoryStore};
use http_body_util::{BodyExt as _, Full};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

pub const CONSOLE_KEY: &str = "console-access-key-for-tests";
pub const ENGINE_KEY: &str = "engine-instance-key-for-tests";

// Literal response bytes emitted by muser@753c2a5. These intentionally do
// not serialize an expected Value with this crate's serde_json settings:
// they catch a source-order regression in those settings.
pub const ENGINE_AUTH_REQUIRED_BODY: &[u8] = br#"{"error":{"type":"authentication_required","message":"a valid bearer API key is required"}}"#;
pub const ENGINE_TLS_REQUIRED_BODY: &[u8] = br#"{"error":{"type":"tls_required","message":"dashboard sessions require HTTPS; use bearer authentication on loopback HTTP"}}"#;
pub const ENGINE_INVALID_STREAM_QUERY_BODY: &[u8] =
    br#"{"error":{"type":"invalid_request_error","message":"unknown query parameter on /stream"}}"#;
pub const ENGINE_WS_TICKET_SHAPE: &str =
    r#"{"ticket":"<ticket>","expires_in":30,"single_use":true}"#;

pub type TestClient = Client<HttpConnector, Full<Bytes>>;

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root must resolve")
}

pub fn scratch_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "{label}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

pub fn write_key_bytes(dir: &Path, name: &str, value: &[u8]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    std::fs::write(&path, value).expect("write key file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod key file");
    path
}

pub fn write_key(dir: &Path, name: &str, value: &str) -> PathBuf {
    write_key_bytes(dir, name, value.as_bytes())
}

pub fn console_config(scratch: &Path, base_url: &str) -> Config {
    let console_key_file = write_key(scratch, "console.key", CONSOLE_KEY);
    let engine_key_file = write_key(scratch, "engine.key", ENGINE_KEY);
    let ui_dir = repo_root().join("ui");
    let text = format!(
        "listen = \"127.0.0.1:0\"\n\
         access_key_file = \"{}\"\n\
         ui_dir = \"{}\"\n\
         \n\
         [[instance]]\n\
         name = \"primary\"\n\
         base_url = \"{}\"\n\
         api_key_file = \"{}\"\n",
        console_key_file.display(),
        ui_dir.display(),
        base_url,
        engine_key_file.display()
    );
    Config::parse(&text, scratch, None).expect("test config must parse")
}

/// TLS-session structural config. The router tests do not terminate TLS, so
/// these bytes are deliberately labelled stimuli rather than certificates;
/// live TLS and certificate validation are exercised by the phase-5
/// acceptance harness.
pub fn console_config_tls(scratch: &Path, base_url: &str, listen: &str) -> Config {
    let console_key_file = write_key(scratch, "console.key", CONSOLE_KEY);
    let engine_key_file = write_key(scratch, "engine.key", ENGINE_KEY);
    let tls_key_file = write_key(scratch, "console-tls.key", "structural private-key input");
    let tls_cert_file = scratch.join("console-tls.crt");
    std::fs::write(&tls_cert_file, b"structural certificate input").expect("write cert input");
    let ui_dir = repo_root().join("ui");
    let text = format!(
        "listen = \"{listen}\"\n\
         tls_cert = \"{}\"\n\
         tls_key = \"{}\"\n\
         access_key_file = \"{}\"\n\
         ui_dir = \"{}\"\n\
         \n\
         [[instance]]\n\
         name = \"primary\"\n\
         base_url = \"{}\"\n\
         api_key_file = \"{}\"\n",
        tls_cert_file.display(),
        tls_key_file.display(),
        console_key_file.display(),
        ui_dir.display(),
        base_url,
        engine_key_file.display()
    );
    Config::parse(&text, scratch, None).expect("TLS structural config must parse")
}

pub async fn spawn_router(application: Router) -> SocketAddr {
    spawn_router_stoppable(application).await.0
}

/// Serves `application` on an address that was already handed out — used to
/// bring a killed stub engine back on the same authority.
pub async fn spawn_router_at(
    address: SocketAddr,
    application: Router,
) -> tokio::task::JoinHandle<()> {
    let mut last = None;
    for _ in 0..50 {
        match tokio::net::TcpListener::bind(address).await {
            Ok(listener) => {
                return tokio::spawn(async move {
                    let _ = axum::serve(
                        listener,
                        application.into_make_service_with_connect_info::<SocketAddr>(),
                    )
                    .await;
                })
            }
            // The aborted stub's listener may not have been reaped yet.
            Err(error) => {
                last = Some(error);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }
    panic!("rebind {address}: {}", last.expect("bind attempted"));
}

/// Like `spawn_router`, but hands back the serve task so a test can kill
/// the stub outright (listener and in-flight connections drop on abort).
pub async fn spawn_router_stoppable(
    application: Router,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let address = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            application.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    (address, handle)
}

pub async fn spawn_console(upstream: SocketAddr) -> SocketAddr {
    let scratch = scratch_dir("console");
    let config = console_config(&scratch, &format!("http://{upstream}"));
    spawn_router(router(AppState::new(config))).await
}

/// Distinct engine key per instance so tests can prove the right key is
/// injected toward the right upstream.
pub fn instance_key(name: &str) -> String {
    format!("engine-instance-key-{name}-for-tests")
}

/// Multi-instance console config; the first entry is the default instance.
pub fn console_config_fleet(scratch: &Path, instances: &[(&str, String)]) -> Config {
    let console_key_file = write_key(scratch, "console.key", CONSOLE_KEY);
    let ui_dir = repo_root().join("ui");
    let mut text = format!(
        "listen = \"127.0.0.1:0\"\naccess_key_file = \"{}\"\nui_dir = \"{}\"\n",
        console_key_file.display(),
        ui_dir.display()
    );
    for (name, base_url) in instances {
        let key_file = write_key(scratch, &format!("{name}.key"), &instance_key(name));
        text.push_str(&format!(
            "\n[[instance]]\nname = \"{name}\"\nbase_url = \"{base_url}\"\napi_key_file = \"{}\"\n",
            key_file.display()
        ));
    }
    Config::parse(&text, scratch, None).expect("fleet test config must parse")
}

pub async fn spawn_console_fleet(instances: &[(&str, SocketAddr)]) -> SocketAddr {
    let scratch = scratch_dir("console-fleet");
    let entries: Vec<(&str, String)> = instances
        .iter()
        .map(|(name, address)| (*name, format!("http://{address}")))
        .collect();
    let config = console_config_fleet(&scratch, &entries);
    spawn_router(router(AppState::new(config))).await
}

// ---------------------------------------------------------------------------
// History-plane consoles

/// A console with the history plane running. Dropping it stops the sampler
/// and maintenance tasks, so one test's sampler can never scrape into
/// another test's store.
pub struct HistoryConsole {
    pub address: SocketAddr,
    pub state: AppState,
    pub db_path: PathBuf,
    workers: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for HistoryConsole {
    fn drop(&mut self) {
        for worker in &self.workers {
            worker.abort();
        }
    }
}

/// Fleet config with a `[history]` table. `sample_interval_ms` overrides
/// the 1 s production cadence; it is not a TOML key, exactly so no
/// deployment can quietly change what "one sample interval" means.
pub fn console_config_history(
    scratch: &Path,
    instances: &[(&str, String)],
    sample_interval_ms: u64,
) -> Config {
    console_config_agents(scratch, instances, &[], sample_interval_ms)
}

/// One `[[agent]]` entry: (agent name, base_url, instance it belongs to, kind).
/// Agents carry no key — the exporters serve no secrets, and the config
/// surface has nowhere to put one.
pub type AgentEntry<'a> = (&'a str, String, &'a str, &'a str);

/// Fleet + history config with `[[agent]]` entries attached.
pub fn console_config_agents(
    scratch: &Path,
    instances: &[(&str, String)],
    agents: &[AgentEntry<'_>],
    sample_interval_ms: u64,
) -> Config {
    let console_key_file = write_key(scratch, "console.key", CONSOLE_KEY);
    let ui_dir = repo_root().join("ui");
    // Deliberately under a directory that does not exist yet: the store has
    // to create its parents.
    let db_path = scratch.join("store").join("console-history.sqlite");
    let mut text = format!(
        "listen = \"127.0.0.1:0\"\n\
         access_key_file = \"{}\"\n\
         ui_dir = \"{}\"\n\
         \n\
         [history]\n\
         db_path = \"{}\"\n\
         retention_days = 7\n\
         enabled = true\n",
        console_key_file.display(),
        ui_dir.display(),
        db_path.display()
    );
    for (name, base_url) in instances {
        let key_file = write_key(scratch, &format!("{name}.key"), &instance_key(name));
        text.push_str(&format!(
            "\n[[instance]]\nname = \"{name}\"\nbase_url = \"{base_url}\"\napi_key_file = \"{}\"\n",
            key_file.display()
        ));
    }
    for (name, base_url, instance, kind) in agents {
        text.push_str(&format!(
            "\n[[agent]]\nname = \"{name}\"\nbase_url = \"{base_url}\"\n\
             instance = \"{instance}\"\nkind = \"{kind}\"\n"
        ));
    }
    let mut config = Config::parse(&text, scratch, None).expect("history test config must parse");
    config.history.sample_interval_ms = sample_interval_ms;
    config
}

pub async fn spawn_console_history(
    instances: &[(&str, SocketAddr)],
    sample_interval_ms: u64,
) -> HistoryConsole {
    spawn_console_agents(instances, &[], sample_interval_ms).await
}

/// A history console with sidecar agents attached. Agent tuples are
/// (agent name, agent address, instance name, kind).
pub async fn spawn_console_agents(
    instances: &[(&str, SocketAddr)],
    agents: &[(&str, SocketAddr, &str, &str)],
    sample_interval_ms: u64,
) -> HistoryConsole {
    let scratch = scratch_dir("console-history");
    let entries: Vec<(&str, String)> = instances
        .iter()
        .map(|(name, address)| (*name, format!("http://{address}")))
        .collect();
    let agent_entries: Vec<AgentEntry<'_>> = agents
        .iter()
        .map(|(name, address, instance, kind)| {
            (*name, format!("http://{address}"), *instance, *kind)
        })
        .collect();
    let config = console_config_agents(&scratch, &entries, &agent_entries, sample_interval_ms);
    let store = HistoryStore::open(&config.history).expect("history store must open");
    let db_path = store.path().to_owned();
    let state = AppState::with_history(config, store);
    let workers = console_server::history::spawn(&state);
    let address = spawn_router(router(state.clone())).await;
    HistoryConsole {
        address,
        state,
        db_path,
        workers,
    }
}

pub fn client() -> TestClient {
    Client::builder(TokioExecutor::new()).build_http()
}

pub async fn request(
    client: &TestClient,
    method: &str,
    address: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (Parts, Bytes) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(format!("http://{address}{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(Full::new(Bytes::copy_from_slice(body)))
        .expect("build request");
    let response = client.request(request).await.expect("request must succeed");
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.expect("collect body").to_bytes();
    (parts, bytes)
}

pub fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

/// Dynamic engine-shaped error used where the message contains a test
/// instance name. The literal constants above are the order oracle.
pub fn engine_error_body(kind: &str, message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"error": {"type": kind, "message": message}}))
        .expect("serialize error body")
}

pub fn fixtures_results_dir() -> PathBuf {
    repo_root().join("fixtures/results")
}

pub const PHASE1_CAPTURE_STEM: &str = "phase1-20260823-attempt4";

pub fn capture_path(suffix: &str) -> PathBuf {
    repo_root()
        .join("fixtures/captures")
        .join(format!("{PHASE1_CAPTURE_STEM}.{suffix}"))
}

/// Literal bytes returned by the live engine's `GET /snapshot`.
pub fn live_engine_snapshot_bytes() -> Vec<u8> {
    std::fs::read(capture_path("engine.snapshot.json")).expect("read live engine snapshot")
}

/// Literal bytes returned by the console proxy for the same live engine.
pub fn live_console_snapshot_bytes() -> Vec<u8> {
    std::fs::read(capture_path("console.snapshot.json")).expect("read live console snapshot")
}

/// Literal text returned by the live engine's `GET /metrics`.
pub fn live_engine_metrics_text() -> String {
    std::fs::read_to_string(capture_path("engine.metrics.txt")).expect("read live engine metrics")
}

/// Sorted RESULT.json fixture paths — real captured bytes, never invented.
pub fn result_fixture_paths() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(fixtures_results_dir())
        .expect("fixtures/results must exist")
        .map(|entry| entry.expect("read dir entry").path())
        .collect();
    dirs.sort();
    let paths: Vec<PathBuf> = dirs
        .into_iter()
        .map(|dir| dir.join("RESULT.json"))
        .filter(|path| path.is_file())
        .collect();
    assert!(!paths.is_empty(), "no RESULT.json fixture present");
    paths
}

/// nth (sorted) historical qualification RESULT.json fixture.
pub fn result_fixture(index: usize) -> Vec<u8> {
    let paths = result_fixture_paths();
    std::fs::read(&paths[index]).expect("read fixture")
}

// ---------------------------------------------------------------------------
// Replay: values and wire bodies read directly from the green live capture.

pub struct Replay {
    /// `/_decode/completion_traffic_tok_s_10s`.
    pub completion_traffic_tok_s: f64,
    /// `/_phases/last_request_decode_tok_s`.
    pub request_decode_tok_s: f64,
    /// `/_queue_depth`.
    pub queue_depth: f64,
    /// `/_decode/completion_tokens`.
    pub completion_tokens: f64,
    /// `/wire/ttft_ms/{p50,p95}`.
    pub ttft_ms_p50: f64,
    pub ttft_ms_p95: f64,
    /// `/wire/itl_ms/p50`.
    pub itl_ms: f64,
    /// `/_phases/<phase>/total_ms`, converted to seconds.
    pub phase_seconds: Vec<(String, f64)>,
    /// `/_phases/prefill/samples`.
    pub prefill_ops: u64,
    /// `/uptime_s`.
    pub wall_s: f64,
    /// `/wire/requests_per_s`.
    pub requests_per_s: f64,
    /// `/wire/ingress_gbps`.
    pub ingress_gbps: f64,
}

fn capture_number(value: &serde_json::Value, pointer: &str) -> f64 {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("live snapshot must carry numeric {pointer}"))
}

pub fn replay() -> Replay {
    let value: serde_json::Value =
        serde_json::from_slice(&live_engine_snapshot_bytes()).expect("live engine snapshot parses");
    let mut phase_seconds = Vec::new();
    for phase in [
        "queue",
        "prefill",
        "sampling",
        "grammar",
        "detokenization",
        "enqueue_write",
        "dflash_draft",
        "dflash_target_verify",
    ] {
        let pointer = format!("/_phases/{phase}/total_ms");
        if let Some(total_ms) = value.pointer(&pointer).and_then(serde_json::Value::as_f64) {
            phase_seconds.push((phase.to_owned(), total_ms / 1000.0));
        }
    }

    Replay {
        completion_traffic_tok_s: capture_number(&value, "/_decode/completion_traffic_tok_s_10s"),
        request_decode_tok_s: capture_number(&value, "/_phases/last_request_decode_tok_s"),
        queue_depth: capture_number(&value, "/_queue_depth"),
        completion_tokens: capture_number(&value, "/_decode/completion_tokens"),
        ttft_ms_p50: capture_number(&value, "/wire/ttft_ms/p50"),
        ttft_ms_p95: capture_number(&value, "/wire/ttft_ms/p95"),
        itl_ms: capture_number(&value, "/wire/itl_ms/p50"),
        phase_seconds,
        prefill_ops: capture_number(&value, "/_phases/prefill/samples") as u64,
        wall_s: capture_number(&value, "/uptime_s"),
        requests_per_s: capture_number(&value, "/wire/requests_per_s"),
        ingress_gbps: capture_number(&value, "/wire/ingress_gbps"),
    }
}

/// The exact Prometheus exposition captured from the live engine.
pub fn replay_metrics_text(_replay: &Replay) -> String {
    live_engine_metrics_text()
}

/// Parsed form of the exact live `/snapshot` capture.
pub fn replay_snapshot(_replay: &Replay) -> serde_json::Value {
    serde_json::from_slice(&live_engine_snapshot_bytes()).expect("live engine snapshot parses")
}

/// The live snapshot as one JSON line, matching an engine SSE/WS data frame.
pub fn telemetry_snapshot_compact() -> String {
    String::from_utf8(live_engine_snapshot_bytes()).expect("live snapshot is UTF-8")
}

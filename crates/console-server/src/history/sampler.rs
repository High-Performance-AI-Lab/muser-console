//! The history sampler: one task per instance, one tick per second.
//!
//! Each tick fetches `GET /metrics` and `GET /snapshot` from the engine, plus
//! `GET /metrics` from every agent attached to that instance, all
//! concurrently over the same hyper client the proxy uses (so upstream
//! connection reuse and per-authority pooling are shared), with a short
//! per-request timeout so a wedged upstream can never hold a tick open. This
//! is a read path only: it touches no proxy state and forwards nothing to
//! the browser.
//!
//! The honesty rules live here, and they are all rules about *not* writing:
//!
//! * a fetch or parse failure records nothing for that source this tick;
//! * a field the engine did not report records nothing;
//! * a field the engine tagged `mock` records nothing;
//! * a device or reading an agent did not publish records nothing;
//! * a non-finite number records nothing.
//!
//! Every gap that results is the truthful record of an upstream that did not
//! report. Nothing is carried forward, interpolated, or zero-filled.
//!
//! Agent samples are stored under the *instance's* name (so the fleet join
//! still works) but with `source = agent` and `honesty = agent-measured`.
//! They are never tagged `measured`: the engine never saw those numbers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::header::{ACCEPT_ENCODING, AUTHORIZATION, HOST};
use axum::http::{HeaderValue, Request, StatusCode};
use http_body_util::BodyExt as _;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::config::Agent;
use crate::history::catalog::{self, Extraction, Honesty, Source, MAX_AGENT_GPUS};
use crate::history::store::Sample;
use crate::logging::log;
use crate::state::{AgentState, AppState};

/// Per-request budget. Two of these run concurrently inside one tick, so a
/// dead instance costs the tick 2 s, never the sampler's cadence.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Budget for all of a tick's agent scrapes together. Deliberately inside
/// the 1 s tick period: an unreachable agent records a gap, it does not
/// slow the engine's own sampling down with it.
pub const AGENT_FETCH_BUDGET: Duration = Duration::from_millis(950);

/// Refuses to buffer an unreasonable upstream body. Engine snapshots are a
/// few kilobytes; this only exists so a misbehaving upstream cannot make
/// the console allocate without bound.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Downsample + retention cadence. `tokio::time::interval` fires its first
/// tick immediately, which is the startup pass.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// Starts one sampler per configured instance plus the maintenance task.
/// Returns the handles so a caller (the tests) can stop them; the server
/// itself just lets them run for the process lifetime.
pub fn spawn(state: &AppState) -> Vec<JoinHandle<()>> {
    if state.history().is_none() {
        return Vec::new();
    }
    let interval = Duration::from_millis(state.config().history.sample_interval_ms);
    let mut handles: Vec<JoinHandle<()>> = state
        .config()
        .instances
        .iter()
        .map(|instance| {
            let state = state.clone();
            let name = instance.name.clone();
            tokio::spawn(sample_loop(state, name, interval))
        })
        .collect();
    handles.push(tokio::spawn(maintenance_loop(state.clone())));
    handles
}

async fn sample_loop(state: AppState, instance_name: String, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    // Delay, not Burst: a slow engine must not make the sampler catch up in
    // a storm of back-to-back scrapes against it.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut metrics_health = Health::default();
    let mut snapshot_health = Health::default();
    let mut carry = Carry::default();
    // One health tracker per attached agent, so an agent that is down for an
    // hour logs once, exactly like an instance that is.
    let mut agent_health: std::collections::HashMap<String, AgentHealth> =
        std::collections::HashMap::new();

    loop {
        ticker.tick().await;
        let Some(store) = state.history() else { return };
        let Some(instance) = state.instance(&instance_name) else {
            return;
        };
        let agents: Vec<&Agent> = state.agents_for(&instance_name).collect();
        // One timestamp for every source in the tick — engine planes and
        // agents alike — so everything a tick collected joins exactly on it.
        let ts_ms = now_ms();
        let (metrics, snapshot, agent_bodies) = tokio::join!(
            fetch(
                state.client(instance),
                &instance.base_url,
                &instance.authority,
                Some(&instance.bearer),
                "/metrics",
            ),
            fetch(
                state.client(instance),
                &instance.base_url,
                &instance.authority,
                Some(&instance.bearer),
                "/snapshot",
            ),
            // No bearer, ever: the exporters serve no secrets, so the console
            // has nothing to prove to them and never hands one a credential.
            //
            // Agents get a budget well inside the tick period. They share
            // this tick with the engine's own two fetches, and a black-holed
            // exporter that burned the full FETCH_TIMEOUT would stretch the
            // tick body past a second and halve the engine's history
            // resolution — an agent must never cost the engine its cadence.
            tokio::time::timeout(
                AGENT_FETCH_BUDGET,
                futures_util::future::join_all(agents.iter().map(|agent| fetch(
                    state.agent_client(),
                    &agent.base_url,
                    &agent.authority,
                    None,
                    "/metrics",
                ))),
            ),
        );
        let agent_bodies = agent_bodies.unwrap_or_else(|_| {
            agents
                .iter()
                .map(|_| Err("timed out".to_owned()))
                .collect::<Vec<_>>()
        });

        // Fetch and parse are one outcome per source: a body that arrives
        // but does not parse is just as unusable as one that never came,
        // and either way this source contributes nothing to this tick.
        let metrics = metrics.and_then(|bytes| {
            String::from_utf8(bytes).map_err(|_| "response was not UTF-8".to_owned())
        });
        let snapshot = snapshot.and_then(|bytes| {
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .map_err(|_| "response was not JSON".to_owned())
        });

        let mut batch: Vec<Sample> = Vec::new();
        let subject = format!("instance '{instance_name}'");
        match &metrics {
            Ok(text) => {
                metrics_health.note(&subject, "/metrics", Ok(()));
                collect_metrics(text, &instance_name, ts_ms, &mut carry, &mut batch);
            }
            Err(reason) => metrics_health.note(&subject, "/metrics", Err(reason)),
        }
        match &snapshot {
            Ok(value) => {
                snapshot_health.note(&subject, "/snapshot", Ok(()));
                collect_snapshot(value, &instance_name, ts_ms, &mut carry, &mut batch);
            }
            Err(reason) => snapshot_health.note(&subject, "/snapshot", Err(reason)),
        }

        // The engine publishes DFlash-shaped zero counters and a zero ratio
        // even when no DFlash runtime exists. `specdec.draft_len` is the
        // engine's explicit runtime-presence marker, so use the successful
        // snapshot to gate *both* planes. If the snapshot is unavailable we
        // cannot honestly claim the metrics-plane zero belongs to a live
        // DFlash runtime either, and record a gap.
        gate_optional_features(snapshot.as_ref().ok(), &mut batch);

        for (agent, body) in agents.iter().zip(agent_bodies) {
            let health = agent_health.entry(agent.name.clone()).or_default();
            collect_agent_tick(
                &state,
                agent,
                health,
                body,
                &instance_name,
                ts_ms,
                &mut batch,
            );
        }

        store.write(batch);
    }
}

/// One agent's half of a tick: reachability, then whatever it published.
fn collect_agent_tick(
    state: &AppState,
    agent: &Agent,
    health: &mut AgentHealth,
    body: Result<Vec<u8>, String>,
    instance: &str,
    ts_ms: i64,
    batch: &mut Vec<Sample>,
) {
    let subject = format!("agent '{}'", agent.name);
    // A body that arrives but is not UTF-8 is as unusable as one that never
    // came, and either way this agent contributes nothing to this tick.
    let text = body.and_then(|bytes| {
        String::from_utf8(bytes).map_err(|_| "response was not UTF-8".to_owned())
    });
    match text {
        Ok(text) => {
            // The scrape succeeded: the exporter is reachable. Whether it had
            // anything to report is a separate question, answered by the
            // series it did (or did not) publish.
            state.set_agent_state(&agent.name, AgentState::Live);
            health.reach.note(&subject, "/metrics", Ok(()));
            let scrape = collect_agent(&text, instance, ts_ms, batch);
            health.note_overflow(&subject, scrape.gpus_seen);
        }
        Err(reason) => {
            state.set_agent_state(&agent.name, AgentState::Unreachable);
            health.reach.note(&subject, "/metrics", Err(&reason));
        }
    }
}

async fn maintenance_loop(state: AppState) {
    let mut ticker = tokio::time::interval(MAINTENANCE_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut health = Health::default();
    loop {
        ticker.tick().await;
        let Some(store) = state.history() else { return };
        match store.maintain_at(now_ms()).await {
            Ok(_) => health.note("history", "maintenance", Ok(())),
            Err(error) => health.note("history", "maintenance", Err(&error.to_string())),
        }
    }
}

/// Tracks up/down so failures log on the state change, not once per tick:
/// an upstream that is down for an hour produces one line, not 3600.
/// `subject` is the already-quoted upstream (`instance 'gx'`, `agent 'g0'`).
#[derive(Default)]
struct Health {
    up: Option<bool>,
}

impl Health {
    fn note(&mut self, subject: &str, what: &str, outcome: Result<(), &str>) {
        let up = outcome.is_ok();
        if self.up == Some(up) {
            return;
        }
        match outcome {
            // Nothing is logged for the very first success: a healthy start
            // is not an event.
            Ok(()) => {
                if self.up.is_some() {
                    log(&format!("history sampler: {subject} {what} recovered"));
                }
            }
            Err(reason) => log(&format!(
                "history sampler: {subject} {what} unavailable ({reason}); \
                 recording nothing until it returns"
            )),
        }
        self.up = Some(up);
    }
}

/// Per-agent tracking: reachability, plus whether the agent published more
/// devices than the catalog can name.
#[derive(Default)]
struct AgentHealth {
    reach: Health,
    /// `Some(true)` while the last scrape overflowed `MAX_AGENT_GPUS`.
    overflowing: Option<bool>,
}

impl AgentHealth {
    /// Logs once per state change when an agent reports more GPUs than the
    /// catalog names. The extra devices are dropped, and saying so is the
    /// only honest option: silently storing eight of eleven would draw a
    /// partial machine as if it were the whole one.
    fn note_overflow(&mut self, subject: &str, gpus_seen: usize) {
        let overflowing = gpus_seen > MAX_AGENT_GPUS;
        if self.overflowing == Some(overflowing) {
            return;
        }
        if overflowing {
            log(&format!(
                "history sampler: {subject} published {gpus_seen} GPUs; \
                 storing the first {MAX_AGENT_GPUS} and recording nothing for the rest"
            ));
        } else if self.overflowing == Some(true) {
            log(&format!(
                "history sampler: {subject} is back within {MAX_AGENT_GPUS} GPUs"
            ));
        }
        self.overflowing = Some(overflowing);
    }
}

/// Fetches one upstream document. An engine's key is injected here exactly
/// as the proxy does it; no error string this returns ever carries it.
/// `bearer` is `None` for agents — there is no agent credential to send, and
/// the console must never offer an engine key or its own to a sidecar.
async fn fetch(
    client: &crate::state::ProxyClient,
    base_url: &str,
    authority: &str,
    bearer: Option<&HeaderValue>,
    path: &str,
) -> Result<Vec<u8>, String> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("{base_url}{path}"))
        .header(HOST, authority)
        // Neither upstream compresses; keep the body identity-encoded so
        // the parser sees exactly what was written.
        .header(ACCEPT_ENCODING, "identity");
    if let Some(bearer) = bearer {
        builder = builder.header(AUTHORIZATION, bearer.clone());
    }
    let request = builder
        .body(Body::empty())
        .map_err(|error| format!("build request: {error}"))?;

    let exchange = async {
        let response = client
            .request(request)
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let body = http_body_util::Limited::new(response.into_body(), MAX_BODY_BYTES);
        let bytes = body
            .collect()
            .await
            .map_err(|_| "response body could not be read".to_owned())?
            .to_bytes();
        if status != StatusCode::OK {
            // Status only: an engine error body may quote the request and
            // must never be echoed into the console's logs.
            return Err(format!("upstream answered {}", status.as_u16()));
        }
        Ok(bytes.to_vec())
    };
    match tokio::time::timeout(FETCH_TIMEOUT, exchange).await {
        Ok(result) => result,
        Err(_) => Err("timed out".to_owned()),
    }
}

/// What the previous tick saw, for the two series whose engine-side value
/// is a latch rather than a fresh reading. Both are gated on evidence that
/// something actually happened since the last tick: re-recording a stale
/// latch once a second would draw a flat line that looks like a live
/// measurement of an event that is long over.
#[derive(Default)]
struct Carry {
    /// `muser_completion_tokens_total` — `request_decode_tok_s` describes
    /// the last completed request and never decays, so it is only a fresh
    /// reading on a tick where this counter advanced.
    completion_tokens: Option<f64>,
    /// Fingerprint of `transfers.last()`. A handoff in flight changes every
    /// tick (bytes_sent advances); a finished one does not.
    transfer_receipt: Option<String>,
}

fn collect_metrics(
    text: &str,
    instance: &str,
    ts_ms: i64,
    carry: &mut Carry,
    batch: &mut Vec<Sample>,
) {
    let exposition = crate::history::prom::Exposition::parse(text);
    let completions = exposition.value("muser_completion_tokens_total", None);
    let completed_this_tick = match (carry.completion_tokens, completions) {
        // Strictly greater: a restarted engine resets the counter, which is
        // not a completed request either.
        (Some(previous), Some(current)) => current > previous,
        // Nothing to compare against yet (first tick, or the counter is
        // absent): no evidence, so no sample.
        _ => false,
    };
    if completions.is_some() {
        carry.completion_tokens = completions;
    }

    for series in catalog::with_source(Source::Metrics) {
        let Extraction::Prometheus { metric, quantile } = series.extraction else {
            continue;
        };
        if series.name == "request_decode_tok_s" && !completed_this_tick {
            continue;
        }
        let Some(value) = exposition.value(metric, quantile) else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }
        batch.push(Sample {
            instance: instance.to_owned(),
            series: series.name,
            ts_ms,
            value,
            source: Source::Metrics,
            // The exposition carries measured series only: mock and target
            // sections are absent from it entirely.
            honesty: Honesty::Measured,
        });
    }
}

/// What one agent scrape contained, beyond the samples it produced.
struct AgentScrape {
    /// Distinct `gpu` label values the exporter published, including any past
    /// `MAX_AGENT_GPUS` that the catalog has no name for.
    gpus_seen: usize,
}

/// Reads one agent exposition into samples.
///
/// Every rule here is a rule about not writing: a device the exporter did not
/// publish, a reading whose NVML probe failed (so the exporter omitted that
/// one line and published the rest), a non-finite number, and — the whole
/// document — an exporter that told us its source did not answer. Samples
/// that do land carry `agent-measured`, never the engine's `measured`.
fn collect_agent(text: &str, instance: &str, ts_ms: i64, batch: &mut Vec<Sample>) -> AgentScrape {
    let exposition = crate::history::prom::Exposition::parse(text);

    // Count devices before deciding anything, so an over-provisioned node is
    // reported even on a tick where nothing was storable.
    let mut gpus: Vec<&str> = Vec::new();
    for sample in exposition.samples() {
        if !sample.metric.starts_with("muser_gpu_") {
            continue;
        }
        if let Some(index) = sample.label("gpu") {
            if !gpus.contains(&index) {
                gpus.push(index);
            }
        }
    }
    let scrape = AgentScrape {
        gpus_seen: gpus.len(),
    };

    // `muser_agent_up 0` is the exporter saying its own data source did not
    // answer this scrape. Anything else in that document would contradict
    // the exporter itself, so the tick stores nothing at all.
    if exposition.value("muser_agent_up", None) == Some(0.0) {
        return scrape;
    }

    for series in catalog::with_source(Source::Agent) {
        let Extraction::Agent { metric, gpu } = series.extraction else {
            continue;
        };
        let value = match gpu {
            Some(index) => exposition.labeled_value(metric, "gpu", index),
            None => exposition.value(metric, None),
        };
        let Some(value) = value else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }
        batch.push(Sample {
            instance: instance.to_owned(),
            series: series.name,
            ts_ms,
            value,
            source: Source::Agent,
            // A different claim from the engine's `measured`, on purpose:
            // this number came from a sidecar the engine cannot see.
            honesty: Honesty::AgentMeasured,
        });
    }
    scrape
}

fn collect_snapshot(
    snapshot: &serde_json::Value,
    instance: &str,
    ts_ms: i64,
    carry: &mut Carry,
    batch: &mut Vec<Sample>,
) {
    // A transfer receipt describes one handoff. The engine keeps the last
    // one in every snapshot, so it is only news while it changes.
    let receipt = snapshot
        .get("transfers")
        .and_then(|value| value.as_array())
        .and_then(|transfers| transfers.last())
        .map(ToString::to_string);
    let receipt_is_new = receipt.is_some() && receipt != carry.transfer_receipt;
    if receipt.is_some() {
        carry.transfer_receipt = receipt;
    }

    for series in catalog::with_source(Source::Snapshot) {
        let Some(path) = series.honesty_path else {
            continue;
        };
        if matches!(series.extraction, Extraction::TransferLast { .. }) && !receipt_is_new {
            continue;
        }
        // Honesty first: a mock or untagged field is skipped before its
        // value is even read, so no code path can store one by accident.
        let Some(honesty) = honesty_for(snapshot, path) else {
            continue;
        };
        let Some(value) = snapshot_value(snapshot, series.extraction) else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }
        batch.push(Sample {
            instance: instance.to_owned(),
            series: series.name,
            ts_ms,
            value,
            source: Source::Snapshot,
            honesty,
        });
    }
}

/// Removes series whose optional runtime was not present this tick.
///
/// This happens after both engine documents have been collected because an
/// optional feature's configuration marker and its measurements can live on
/// different planes. A missing marker is unavailable, never a measured zero.
fn gate_optional_features(snapshot: Option<&serde_json::Value>, batch: &mut Vec<Sample>) {
    let dflash_configured = snapshot
        .and_then(|value| json_at(value, "specdec.draft_len"))
        .and_then(serde_json::Value::as_u64)
        .is_some();
    if !dflash_configured {
        batch.retain(|sample| !sample.series.starts_with("dflash_"));
    }
}

/// Reads a tag out of the snapshot's `_honesty` sidecar.
///
/// The sidecar mirrors the snapshot tree, so a section may be tagged as a
/// whole (`_honesty.specdec = "measured"`) or field by field
/// (`_honesty.wire.requests_per_s`). The exact path is tried first, then
/// successively shorter prefixes — the closest tag that actually exists
/// wins.
///
/// `mock` yields `None`, and so does a path with no tag at all, with one
/// documented exception: `_`-prefixed process extensions carry no sidecar
/// entry and are measured per the engine docs.
fn honesty_for(snapshot: &serde_json::Value, path: &str) -> Option<Honesty> {
    let segments: Vec<&str> = path.split('.').collect();
    if let Some(sidecar) = snapshot.get("_honesty") {
        for end in (1..=segments.len()).rev() {
            let mut current = sidecar;
            let mut found = true;
            for segment in &segments[..end] {
                match current.get(segment) {
                    Some(next) => current = next,
                    None => {
                        found = false;
                        break;
                    }
                }
            }
            if found {
                if let Some(tag) = current.as_str() {
                    return Honesty::from_tag(tag);
                }
            }
        }
    }
    if segments.first().is_some_and(|first| first.starts_with('_')) {
        Some(Honesty::Measured)
    } else {
        None
    }
}

fn snapshot_value(snapshot: &serde_json::Value, extraction: Extraction) -> Option<f64> {
    match extraction {
        Extraction::Snapshot { path } => json_at(snapshot, path)?.as_f64(),
        Extraction::TransfersActive => {
            let transfers = snapshot.get("transfers")?.as_array()?;
            let active = transfers
                .iter()
                .filter(|entry| entry.get("phase").and_then(|p| p.as_str()) != Some("done"))
                .count();
            Some(active as f64)
        }
        Extraction::TransferLast { field, scale } => {
            let transfers = snapshot.get("transfers")?.as_array()?;
            // Empty transfers means no handoff to describe — not a zero.
            let last = transfers.last()?;
            Some(last.get(field)?.as_f64()? * scale)
        }
        // Neither of these reads the snapshot: one is the engine's
        // exposition, the other a sidecar's.
        Extraction::Prometheus { .. } | Extraction::Agent { .. } => None,
    }
}

fn json_at<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = root;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(batch: &[Sample]) -> Vec<&'static str> {
        batch.iter().map(|sample| sample.series).collect()
    }

    fn find<'a>(batch: &'a [Sample], name: &str) -> Option<&'a Sample> {
        batch.iter().find(|sample| sample.series == name)
    }

    #[test]
    fn honesty_reads_the_exact_path_then_the_enclosing_section() {
        let snapshot = json!({
            "_honesty": {
                "wire": {"requests_per_s": "measured", "egress_gbps": "mock"},
                "specdec": "measured",
                "transfers": "target"
            }
        });
        assert_eq!(
            honesty_for(&snapshot, "wire.requests_per_s"),
            Some(Honesty::Measured)
        );
        assert_eq!(
            honesty_for(&snapshot, "specdec"),
            Some(Honesty::Measured),
            "a whole-section tag covers its fields"
        );
        assert_eq!(honesty_for(&snapshot, "transfers"), Some(Honesty::Target));
        assert_eq!(
            honesty_for(&snapshot, "wire.egress_gbps"),
            None,
            "mock is never a storable tag"
        );
        assert_eq!(
            honesty_for(&snapshot, "wire.ingress_gbps"),
            None,
            "an untagged field under a tagged section object is unknown, so it is skipped"
        );
        assert_eq!(honesty_for(&snapshot, "economics.counters"), None);
    }

    #[test]
    fn underscore_extensions_without_a_tag_are_measured() {
        let snapshot = json!({"_honesty": {"wire": {"egress_gbps": "mock"}}});
        assert_eq!(honesty_for(&snapshot, "_remote"), Some(Honesty::Measured));
        assert_eq!(honesty_for(&snapshot, "economics.counters"), None);
        // A sidecar that does tag an underscore section still wins.
        let tagged = json!({"_honesty": {"_remote": "mock"}});
        assert_eq!(honesty_for(&tagged, "_remote"), None);
    }

    #[test]
    fn missing_sidecar_skips_everything_except_underscore_extensions() {
        let snapshot = json!({"wire": {"requests_per_s": 1.0}});
        assert_eq!(honesty_for(&snapshot, "wire.requests_per_s"), None);
        assert_eq!(honesty_for(&snapshot, "_remote"), Some(Honesty::Measured));
    }

    #[test]
    fn mock_tagged_snapshot_fields_are_never_stored() {
        let snapshot = json!({
            "wire": {"requests_per_s": 2.0, "ingress_gbps": 3.0, "egress_gbps": 4.0},
            "transfers": [],
            "_honesty": {
                "wire": {"requests_per_s": "measured", "ingress_gbps": "mock", "egress_gbps": "mock"},
                "transfers": "measured"
            }
        });
        let mut batch = Vec::new();
        collect_snapshot(&snapshot, "gx", 1_000, &mut Carry::default(), &mut batch);
        assert!(names(&batch).contains(&"requests_per_s"));
        assert!(
            !names(&batch).contains(&"wire_ingress_gbps"),
            "a mock-tagged field yields no series at all"
        );
        assert_eq!(
            find(&batch, "transfers_active").map(|sample| sample.value),
            Some(0.0),
            "an empty transfers array is a measured zero, not a gap"
        );
    }

    #[test]
    fn transfer_last_series_only_exist_while_a_transfer_does() {
        let empty = json!({"transfers": [], "_honesty": {"transfers": "measured"}});
        let mut batch = Vec::new();
        collect_snapshot(&empty, "gx", 1_000, &mut Carry::default(), &mut batch);
        assert!(!names(&batch).contains(&"transfer_last_bytes_total"));

        let one = json!({
            "transfers": [{
                "session": "s", "src_node": "gx10-0", "dst_node": "m3ultra-0",
                "bytes_total": 4096, "bytes_sent": 4096, "phase": "done",
                "throughput_gbps": 3.5, "hidden_pct": 0.5,
                "_control_ns": 2_500_000, "_accept_ns": 1_000_000
            }],
            "_honesty": {"transfers": "measured"}
        });
        let mut batch = Vec::new();
        collect_snapshot(&one, "gx", 1_000, &mut Carry::default(), &mut batch);
        assert_eq!(
            find(&batch, "transfer_last_bytes_total").map(|s| s.value),
            Some(4096.0)
        );
        assert_eq!(
            find(&batch, "transfer_last_control_ms").map(|s| s.value),
            Some(2.5),
            "_control_ns is stored as milliseconds"
        );
        assert_eq!(
            find(&batch, "transfer_last_accept_ms").map(|s| s.value),
            Some(1.0)
        );
        assert_eq!(
            find(&batch, "transfers_active").map(|s| s.value),
            Some(0.0),
            "a done transfer is not active"
        );

        let streaming = json!({
            "transfers": [{
                "session": "s", "src_node": "gx10-0", "dst_node": "m3ultra-0",
                "bytes_total": 4096, "bytes_sent": 100, "phase": "streaming_nope"
            }],
            "_honesty": {"transfers": "measured"}
        });
        let mut batch = Vec::new();
        collect_snapshot(&streaming, "gx", 1_000, &mut Carry::default(), &mut batch);
        assert_eq!(find(&batch, "transfers_active").map(|s| s.value), Some(1.0));
        assert!(
            !names(&batch).contains(&"transfer_last_throughput_gbps"),
            "a field the transfer does not carry yields no series"
        );
    }

    #[test]
    fn non_finite_and_non_numeric_values_are_skipped() {
        let snapshot = json!({
            "wire": {"requests_per_s": "not-a-number"},
            "_honesty": {"wire": {"requests_per_s": "measured"}}
        });
        let mut batch = Vec::new();
        collect_snapshot(&snapshot, "gx", 1_000, &mut Carry::default(), &mut batch);
        assert!(batch.is_empty());

        let mut batch = Vec::new();
        collect_metrics(
            "muser_queue_depth NaN\n",
            "gx",
            1_000,
            &mut Carry::default(),
            &mut batch,
        );
        assert!(batch.is_empty(), "NaN is not a measurement");
    }

    #[test]
    fn dflash_history_requires_the_runtime_presence_marker() {
        let without_runtime = json!({
            "specdec": {
                "accept_rate": 0.0,
                "cumulative_accepted": 0,
                "cumulative_drafted": 0,
                "ane_route_failures": 0,
                "metal_route_failures": 0
            },
            "_honesty": {"specdec": "measured"}
        });
        let mut carry = Carry::default();
        let mut batch = Vec::new();
        collect_metrics(
            "muser_queue_depth 0\nmuser_dflash_acceptance_ratio 0\n",
            "gx",
            1_000,
            &mut carry,
            &mut batch,
        );
        collect_snapshot(&without_runtime, "gx", 1_000, &mut carry, &mut batch);
        gate_optional_features(Some(&without_runtime), &mut batch);
        assert!(
            names(&batch)
                .iter()
                .all(|name| !name.starts_with("dflash_")),
            "unconfigured DFlash values are gaps, not a row of synthetic-looking zeros"
        );
        assert_eq!(
            find(&batch, "queue_depth").map(|sample| sample.value),
            Some(0.0),
            "gating one optional feature must not discard ordinary measurements"
        );

        let with_runtime = json!({
            "specdec": {
                "draft_len": 7,
                "accept_rate": 0.0,
                "cumulative_accepted": 0,
                "cumulative_drafted": 0,
                "ane_route_failures": 0,
                "metal_route_failures": 0
            },
            "_honesty": {"specdec": "measured"}
        });
        let mut batch = Vec::new();
        collect_metrics(
            "muser_dflash_acceptance_ratio 0\n",
            "gx",
            2_000,
            &mut Carry::default(),
            &mut batch,
        );
        collect_snapshot(
            &with_runtime,
            "gx",
            2_000,
            &mut Carry::default(),
            &mut batch,
        );
        gate_optional_features(Some(&with_runtime), &mut batch);
        assert_eq!(
            find(&batch, "dflash_accept_rate").map(|sample| sample.value),
            Some(0.0),
            "a configured runtime's measured zero remains a real measurement"
        );
        assert_eq!(
            find(&batch, "dflash_drafted_total").map(|sample| sample.value),
            Some(0.0)
        );
    }

    #[test]
    fn last_request_decode_rate_is_only_recorded_when_a_request_completed() {
        // The engine's muser_request_decode_tok_s describes the last
        // completed request and never decays. Storing it every tick would
        // draw a flat live-looking line for a request that ended long ago.
        let mut carry = Carry::default();
        let idle = "muser_completion_tokens_total 100\nmuser_request_decode_tok_s 107.9\n";

        let mut batch = Vec::new();
        collect_metrics(idle, "gx", 1_000, &mut carry, &mut batch);
        assert!(
            find(&batch, "request_decode_tok_s").is_none(),
            "the first tick has nothing to compare against, so it claims nothing"
        );

        let mut batch = Vec::new();
        collect_metrics(idle, "gx", 2_000, &mut carry, &mut batch);
        assert!(
            find(&batch, "request_decode_tok_s").is_none(),
            "no completion since the last tick: the latch is stale, not a reading"
        );
        assert_eq!(
            find(&batch, "completion_tokens_total").map(|s| s.value),
            Some(100.0),
            "the counter itself is still a real measurement every tick"
        );

        let mut batch = Vec::new();
        collect_metrics(
            "muser_completion_tokens_total 356\nmuser_request_decode_tok_s 107.9\n",
            "gx",
            3_000,
            &mut carry,
            &mut batch,
        );
        assert_eq!(
            find(&batch, "request_decode_tok_s").map(|s| s.value),
            Some(107.9),
            "a request completed this tick, so the rate describes it"
        );

        let mut batch = Vec::new();
        collect_metrics(
            "muser_completion_tokens_total 1\nmuser_request_decode_tok_s 107.9\n",
            "gx",
            4_000,
            &mut carry,
            &mut batch,
        );
        assert!(
            find(&batch, "request_decode_tok_s").is_none(),
            "a restarted engine resets the counter; that is not a completion"
        );
    }

    #[test]
    fn a_transfer_receipt_is_recorded_once_per_change_not_once_per_tick() {
        // The engine keeps the last handoff in every snapshot. Re-recording
        // it each second would draw a live series for a finished event.
        let done = json!({
            "transfers": [{
                "session": "s", "src_node": "gx10-0", "dst_node": "m3ultra-0",
                "bytes_total": 4096, "bytes_sent": 4096, "phase": "done",
                "throughput_gbps": 3.5, "hidden_pct": 0.5,
                "_control_ns": 2_500_000, "_accept_ns": 1_000_000
            }],
            "_honesty": {"transfers": "measured"}
        });
        let mut carry = Carry::default();

        let mut batch = Vec::new();
        collect_snapshot(&done, "gx", 1_000, &mut carry, &mut batch);
        assert_eq!(
            find(&batch, "transfer_last_bytes_total").map(|s| s.value),
            Some(4096.0),
            "a receipt not seen before is news"
        );

        let mut batch = Vec::new();
        collect_snapshot(&done, "gx", 2_000, &mut carry, &mut batch);
        assert!(
            !names(&batch).contains(&"transfer_last_bytes_total"),
            "the same finished handoff is not a new measurement a second later"
        );
        assert_eq!(
            find(&batch, "transfers_active").map(|s| s.value),
            Some(0.0),
            "the active count is a fresh reading every tick regardless"
        );

        // A handoff in flight advances, so it keeps producing points.
        let advancing = json!({
            "transfers": [{
                "session": "s2", "src_node": "gx10-0", "dst_node": "m3ultra-0",
                "bytes_total": 4096, "bytes_sent": 2048, "phase": "streaming_nope",
                "throughput_gbps": 3.5, "hidden_pct": 0.5,
                "_control_ns": 2_500_000, "_accept_ns": 1_000_000
            }],
            "_honesty": {"transfers": "measured"}
        });
        let mut batch = Vec::new();
        collect_snapshot(&advancing, "gx", 3_000, &mut carry, &mut batch);
        assert_eq!(
            find(&batch, "transfer_last_bytes_total").map(|s| s.value),
            Some(4096.0),
            "a different receipt is news again"
        );
    }

    // ---- agent exporters -------------------------------------------------
    //
    // The expositions below are structural plumbing, not telemetry: the
    // values are ordinals chosen to be distinguishable from one another and
    // nothing here is ever rendered. What is asserted is which series exist,
    // which device they came from, and how they are tagged — never that any
    // number means anything.

    #[test]
    fn agent_samples_carry_agent_measured_and_their_own_device() {
        let text = "\
# HELP muser_agent_up whether the source answered this scrape
# TYPE muser_agent_up gauge
muser_agent_up{agent=\"gx10\"} 1
# TYPE muser_gpu_utilization_ratio gauge
muser_gpu_utilization_ratio{gpu=\"0\",uuid=\"GPU-aaa\",name=\"Dev A\"} 0.25
muser_gpu_utilization_ratio{gpu=\"1\",uuid=\"GPU-bbb\",name=\"Dev B\"} 0.75
muser_gpu_power_watts{gpu=\"0\",uuid=\"GPU-aaa\",name=\"Dev A\"} 11
muser_gpu_memory_used_bytes{gpu=\"0\",uuid=\"GPU-aaa\",name=\"Dev A\"} 1024
muser_gpu_memory_total_bytes{gpu=\"0\",uuid=\"GPU-aaa\",name=\"Dev A\"} 4096
";
        let mut batch = Vec::new();
        let scrape = collect_agent(text, "gx", 1_000, &mut batch);
        assert_eq!(scrape.gpus_seen, 2);

        let util0 = find(&batch, "gpu0_utilization_ratio").expect("gpu0 utilization");
        assert_eq!(util0.value, 0.25);
        assert_eq!(util0.source, Source::Agent);
        assert_eq!(
            util0.honesty,
            Honesty::AgentMeasured,
            "an agent number must never be tagged with the engine's 'measured'"
        );
        assert_eq!(
            util0.instance, "gx",
            "stored under the instance it belongs to"
        );
        assert_eq!(util0.ts_ms, 1_000);
        assert_eq!(
            find(&batch, "gpu1_utilization_ratio").map(|s| s.value),
            Some(0.75),
            "each device's series reads its own gpu label"
        );

        // The exporter published no power/temp/memory for device 1 and no
        // temperature for device 0: each missing reading is a gap of its own,
        // and the readings that were published still land.
        assert_eq!(
            find(&batch, "gpu0_power_watts").map(|s| s.value),
            Some(11.0)
        );
        assert!(
            find(&batch, "gpu0_temperature_celsius").is_none(),
            "a probe the exporter omitted stores nothing for that field alone"
        );
        assert_eq!(
            find(&batch, "gpu0_memory_used_bytes").map(|s| s.value),
            Some(1024.0)
        );
        assert!(find(&batch, "gpu1_power_watts").is_none());
        assert!(find(&batch, "gpu2_utilization_ratio").is_none());
        assert!(
            find(&batch, "host_package_power_watts").is_none(),
            "a GPU exporter publishes no host power, so none is stored"
        );
    }

    #[test]
    fn an_exporter_that_says_its_source_is_down_stores_nothing() {
        // muser_agent_up 0 is the exporter disclaiming its own data source.
        // Any device line in that same document contradicts it, so the whole
        // tick is a gap rather than a number nobody stands behind.
        let mut batch = Vec::new();
        let scrape = collect_agent(
            "muser_agent_up{agent=\"gx10\"} 0\n",
            "gx",
            1_000,
            &mut batch,
        );
        assert!(batch.is_empty());
        assert_eq!(scrape.gpus_seen, 0);

        let mut batch = Vec::new();
        collect_agent(
            "muser_agent_up{agent=\"gx10\"} 0\nmuser_gpu_utilization_ratio{gpu=\"0\"} 0.5\n",
            "gx",
            1_000,
            &mut batch,
        );
        assert!(
            batch.is_empty(),
            "the exporter's own up=0 wins over any series in the same document"
        );
    }

    #[test]
    fn host_power_series_are_stored_only_where_the_sampler_reported() {
        let text = "\
muser_agent_up{agent=\"mac\"} 1
muser_host_package_power_watts{host=\"studio\"} 7
muser_host_cpu_power_watts{host=\"studio\"} 3
";
        let mut batch = Vec::new();
        let scrape = collect_agent(text, "mac", 2_000, &mut batch);
        assert_eq!(scrape.gpus_seen, 0);
        let package = find(&batch, "host_package_power_watts").expect("package power");
        assert_eq!(package.value, 7.0);
        assert_eq!(package.honesty, Honesty::AgentMeasured);
        assert_eq!(
            find(&batch, "host_cpu_power_watts").map(|s| s.value),
            Some(3.0)
        );
        assert!(
            find(&batch, "host_gpu_power_watts").is_none(),
            "a sampler that did not report is a gap, never a zero"
        );
    }

    #[test]
    fn devices_past_the_catalog_are_reported_rather_than_silently_dropped() {
        let mut text = String::from("muser_agent_up{agent=\"gx10\"} 1\n");
        // One more device than the catalog can name.
        for index in 0..=MAX_AGENT_GPUS {
            text.push_str(&format!(
                "muser_gpu_utilization_ratio{{gpu=\"{index}\"}} 0.5\n"
            ));
        }
        let mut batch = Vec::new();
        let scrape = collect_agent(&text, "gx", 1_000, &mut batch);
        assert_eq!(scrape.gpus_seen, MAX_AGENT_GPUS + 1);
        assert!(
            scrape.gpus_seen > MAX_AGENT_GPUS,
            "the overflow has to be visible to the caller that logs it"
        );
        assert_eq!(
            batch.len(),
            MAX_AGENT_GPUS,
            "the catalog stores the devices it can name and no more"
        );
        assert!(find(&batch, "gpu7_utilization_ratio").is_some());

        let mut health = AgentHealth::default();
        health.note_overflow("agent 'g'", scrape.gpus_seen);
        assert_eq!(health.overflowing, Some(true));
        health.note_overflow("agent 'g'", scrape.gpus_seen);
        assert_eq!(
            health.overflowing,
            Some(true),
            "logging is once per state change, not once per tick"
        );
        health.note_overflow("agent 'g'", MAX_AGENT_GPUS);
        assert_eq!(health.overflowing, Some(false));
    }

    #[test]
    fn non_finite_and_unparseable_agent_lines_store_nothing() {
        let text = "\
muser_agent_up{agent=\"gx10\"} 1
muser_gpu_utilization_ratio{gpu=\"0\"} NaN
muser_gpu_power_watts{gpu=\"0\"} not-a-number
muser_gpu_temperature_celsius{gpu=\"0\"} +Inf
";
        let mut batch = Vec::new();
        collect_agent(text, "gx", 1_000, &mut batch);
        assert!(batch.is_empty(), "NaN and +Inf are not measurements");
    }

    #[test]
    fn an_empty_agent_document_stores_nothing() {
        let mut batch = Vec::new();
        let scrape = collect_agent("", "gx", 1_000, &mut batch);
        assert!(batch.is_empty());
        assert_eq!(scrape.gpus_seen, 0);
    }

    #[test]
    fn metrics_samples_are_tagged_measured_from_the_metrics_plane() {
        let mut batch = Vec::new();
        collect_metrics(
            "muser_queue_depth 2\nmuser_ttft_milliseconds{quantile=\"0.95\"} 12.5\n",
            "gx",
            7,
            &mut Carry::default(),
            &mut batch,
        );
        let queue = find(&batch, "queue_depth").expect("queue_depth sampled");
        assert_eq!(queue.value, 2.0);
        assert_eq!(queue.source, Source::Metrics);
        assert_eq!(queue.honesty, Honesty::Measured);
        assert_eq!(queue.ts_ms, 7);
        assert_eq!(find(&batch, "ttft_ms_p95").map(|s| s.value), Some(12.5));
        assert!(
            find(&batch, "ttft_ms_p50").is_none(),
            "an unpublished quantile is a gap, not a copy of p95"
        );
    }
}

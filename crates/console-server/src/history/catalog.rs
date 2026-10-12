//! The fixed history series catalog: every series the console can store,
//! where its value comes from, and where its honesty tag is read.
//!
//! One module, one table. Nothing is stored that is not listed here, and
//! nothing listed here is ever synthesized: when a source field is absent,
//! unparseable, or tagged `mock`, the tick records no sample for it. A gap
//! in the store is the honest record of "the engine did not report this".
//!
//! Where a field exists in both planes (queue depth, overload rejections,
//! decode traffic, dflash accept rate) the Prometheus exposition wins and
//! the snapshot copy is not stored — one series, one source, no duplicates.
//! `series_names_are_unique` in the tests below pins that.

/// Counter vs gauge decides how a series is aggregated when the store
/// downsamples or the query API buckets: a gauge averages, a counter keeps
/// the bucket's last value (a cumulative total must never be averaged).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Gauge,
    Counter,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Gauge => "gauge",
            Kind::Counter => "counter",
        }
    }
}

/// Which read path produced the sample. Stored on every row so a chart can
/// badge where its numbers came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// `GET /metrics` — the engine's Prometheus exposition.
    Metrics,
    /// `GET /snapshot` — the engine's `MetricsSnapshot` JSON.
    Snapshot,
    /// A sidecar exporter's `GET /metrics` (NVML / powermetrics). Not the
    /// engine: these series exist precisely because the engine cannot see
    /// the hardware they describe.
    Agent,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Metrics => "metrics",
            Source::Snapshot => "snapshot",
            Source::Agent => "agent",
        }
    }

    /// Inverse of `as_str`: reads the vocabulary as it is stored on a row.
    pub fn from_stored(text: &str) -> Option<Source> {
        match text {
            "metrics" => Some(Source::Metrics),
            "snapshot" => Some(Source::Snapshot),
            "agent" => Some(Source::Agent),
            _ => None,
        }
    }
}

/// The honesty vocabulary that survives into the store. The engine's third
/// tag, `mock`, deliberately has no variant: a mock-tagged field is never
/// stored at all, so no row can ever claim it was measured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Honesty {
    /// The engine reported it and tagged it measured.
    Measured,
    Target,
    /// A sidecar exporter measured it. Deliberately a separate value from
    /// `Measured`: the engine never saw this number, and a UI that badges
    /// the two the same would be claiming it did.
    AgentMeasured,
}

impl Honesty {
    pub fn as_str(self) -> &'static str {
        match self {
            Honesty::Measured => "measured",
            Honesty::Target => "target",
            Honesty::AgentMeasured => "agent-measured",
        }
    }

    /// `mock` maps to `None` — the caller must skip the sample entirely.
    pub fn from_tag(tag: &str) -> Option<Honesty> {
        match tag {
            "measured" => Some(Honesty::Measured),
            "target" => Some(Honesty::Target),
            "agent-measured" => Some(Honesty::AgentMeasured),
            _ => None,
        }
    }
}

/// How a tick's payload yields this series' value.
#[derive(Clone, Copy, Debug)]
pub enum Extraction {
    /// A Prometheus exposition metric, optionally selected by its
    /// `quantile` label.
    Prometheus {
        metric: &'static str,
        quantile: Option<&'static str>,
    },
    /// A dotted path into the snapshot JSON, e.g. `wire.requests_per_s`.
    Snapshot { path: &'static str },
    /// Count of `transfers[]` entries whose `phase` is not `"done"`.
    TransfersActive,
    /// A field of the last `transfers[]` entry, scaled (ns -> ms). Sampled
    /// only while `transfers` is non-empty.
    TransferLast { field: &'static str, scale: f64 },
    /// A sidecar exporter's Prometheus metric, selected by its `gpu` label
    /// where the exporter publishes one series per device.
    Agent {
        metric: &'static str,
        gpu: Option<&'static str>,
    },
}

pub struct Series {
    pub name: &'static str,
    pub kind: Kind,
    pub source: Source,
    pub unit: &'static str,
    pub extraction: Extraction,
    /// Dotted path under the snapshot's `_honesty` sidecar. `None` for
    /// `/metrics` series: the exposition carries no honesty labels because
    /// it only ever contains measured series (mock/target sections are
    /// simply absent from it) — see docs/engine-contract.md.
    pub honesty_path: Option<&'static str>,
}

const fn prom(
    name: &'static str,
    kind: Kind,
    unit: &'static str,
    metric: &'static str,
    quantile: Option<&'static str>,
) -> Series {
    Series {
        name,
        kind,
        source: Source::Metrics,
        unit,
        extraction: Extraction::Prometheus { metric, quantile },
        honesty_path: None,
    }
}

const fn snap(
    name: &'static str,
    kind: Kind,
    unit: &'static str,
    path: &'static str,
    honesty_path: &'static str,
) -> Series {
    Series {
        name,
        kind,
        source: Source::Snapshot,
        unit,
        extraction: Extraction::Snapshot { path },
        honesty_path: Some(honesty_path),
    }
}

const fn transfer_last(
    name: &'static str,
    unit: &'static str,
    field: &'static str,
    scale: f64,
) -> Series {
    Series {
        name,
        kind: Kind::Gauge,
        source: Source::Snapshot,
        unit,
        extraction: Extraction::TransferLast { field, scale },
        honesty_path: Some("transfers"),
    }
}

/// The catalog. Order is the order the `/series` listing reports.
pub static CATALOG: &[Series] = &[
    // ---- GET /metrics (Prometheus exposition; measured-only by contract) --
    // Note the missing muser_ prefix on the first one: a known engine wart,
    // documented in docs/engine-contract.md, matched here exactly.
    prom(
        "decode_tok_s",
        Kind::Gauge,
        "tok/s",
        "completion_traffic_tok_s_10s",
        None,
    ),
    prom(
        "request_decode_tok_s",
        Kind::Gauge,
        "tok/s",
        "muser_request_decode_tok_s",
        None,
    ),
    prom(
        "queue_depth",
        Kind::Gauge,
        "requests",
        "muser_queue_depth",
        None,
    ),
    prom(
        "overload_rejections_total",
        Kind::Counter,
        "requests",
        "muser_overload_rejections_total",
        None,
    ),
    prom(
        "completion_tokens_total",
        Kind::Counter,
        "tokens",
        "muser_completion_tokens_total",
        None,
    ),
    prom(
        "ttft_ms_p50",
        Kind::Gauge,
        "ms",
        "muser_ttft_milliseconds",
        Some("0.50"),
    ),
    prom(
        "ttft_ms_p95",
        Kind::Gauge,
        "ms",
        "muser_ttft_milliseconds",
        Some("0.95"),
    ),
    prom(
        "itl_ms_p50",
        Kind::Gauge,
        "ms",
        "muser_itl_milliseconds",
        Some("0.50"),
    ),
    prom(
        "itl_ms_p95",
        Kind::Gauge,
        "ms",
        "muser_itl_milliseconds",
        Some("0.95"),
    ),
    prom(
        "dflash_accept_rate",
        Kind::Gauge,
        "ratio",
        "muser_dflash_acceptance_ratio",
        None,
    ),
    // ---- GET /snapshot (honesty read from the _honesty sidecar) ----------
    snap(
        "requests_per_s",
        Kind::Gauge,
        "req/s",
        "wire.requests_per_s",
        "wire.requests_per_s",
    ),
    snap(
        "wire_ingress_gbps",
        Kind::Gauge,
        "Gb/s",
        "wire.ingress_gbps",
        "wire.ingress_gbps",
    ),
    snap(
        "dflash_drafted_total",
        Kind::Counter,
        "tokens",
        "specdec.cumulative_drafted",
        "specdec",
    ),
    snap(
        "dflash_accepted_total",
        Kind::Counter,
        "tokens",
        "specdec.cumulative_accepted",
        "specdec",
    ),
    snap(
        "dflash_ane_route_failures_total",
        Kind::Counter,
        "attempts",
        "specdec.ane_route_failures",
        "specdec",
    ),
    snap(
        "dflash_metal_route_failures_total",
        Kind::Counter,
        "attempts",
        "specdec.metal_route_failures",
        "specdec",
    ),
    // `_remote` is a process extension: the engine documents underscore
    // sections without an _honesty entry as measured.
    snap(
        "remote_fallbacks_total",
        Kind::Counter,
        "requests",
        "_remote.fallbacks",
        "_remote",
    ),
    Series {
        name: "transfers_active",
        kind: Kind::Gauge,
        source: Source::Snapshot,
        unit: "transfers",
        extraction: Extraction::TransfersActive,
        honesty_path: Some("transfers"),
    },
    transfer_last("transfer_last_bytes_total", "bytes", "bytes_total", 1.0),
    transfer_last(
        "transfer_last_throughput_gbps",
        "Gb/s",
        "throughput_gbps",
        1.0,
    ),
    transfer_last("transfer_last_hidden_pct", "ratio", "hidden_pct", 1.0),
    transfer_last("transfer_last_control_ms", "ms", "_control_ns", 1e-6),
    transfer_last("transfer_last_accept_ms", "ms", "_accept_ns", 1e-6),
    snap(
        "disagg_prefills_total",
        Kind::Counter,
        "prefills",
        "economics.disagg_prefills",
        "economics.counters",
    ),
    snap(
        "disagg_bytes_installed_total",
        Kind::Counter,
        "bytes",
        "economics.disagg_bytes_installed",
        "economics.counters",
    ),
];

// ---------------------------------------------------------------------------
// Agent series (phase 4)
//
// The store's key is (instance, series, ts), so a per-device series has to
// carry the device in its *name* — hence gpu0_…, gpu1_…, rather than one
// series with a label. That means the catalog has to name a fixed number of
// devices up front: `MAX_AGENT_GPUS`. A node with more GPUs than that gets
// the first `MAX_AGENT_GPUS` stored and a log line naming what was dropped —
// never a silently truncated fleet view.
//
// None of these carry an `honesty_path`: there is no snapshot sidecar behind
// them. Their honesty is `agent-measured`, written by the sampler, and it is
// a different claim from the engine's `measured` on purpose.

/// How many GPUs one agent can publish before the console runs out of names.
pub const MAX_AGENT_GPUS: usize = 8;

const fn agent_gpu(
    name: &'static str,
    unit: &'static str,
    metric: &'static str,
    gpu: &'static str,
) -> Series {
    Series {
        name,
        kind: Kind::Gauge,
        source: Source::Agent,
        unit,
        extraction: Extraction::Agent {
            metric,
            gpu: Some(gpu),
        },
        honesty_path: None,
    }
}

const fn agent_host(name: &'static str, unit: &'static str, metric: &'static str) -> Series {
    Series {
        name,
        kind: Kind::Gauge,
        source: Source::Agent,
        unit,
        extraction: Extraction::Agent { metric, gpu: None },
        honesty_path: None,
    }
}

/// Expands the per-device block once per GPU index. The index is spliced
/// into both the series name and the `gpu` label value it selects on, so the
/// two can never drift apart.
macro_rules! agent_catalog {
    ($($index:literal),+ $(,)?) => {
        &[
            $(
                agent_gpu(
                    concat!("gpu", $index, "_utilization_ratio"),
                    "ratio",
                    "muser_gpu_utilization_ratio",
                    $index,
                ),
                agent_gpu(
                    concat!("gpu", $index, "_power_watts"),
                    "W",
                    "muser_gpu_power_watts",
                    $index,
                ),
                agent_gpu(
                    concat!("gpu", $index, "_temperature_celsius"),
                    "°C",
                    "muser_gpu_temperature_celsius",
                    $index,
                ),
                agent_gpu(
                    concat!("gpu", $index, "_memory_used_bytes"),
                    "bytes",
                    "muser_gpu_memory_used_bytes",
                    $index,
                ),
                agent_gpu(
                    concat!("gpu", $index, "_memory_total_bytes"),
                    "bytes",
                    "muser_gpu_memory_total_bytes",
                    $index,
                ),
            )+
            // Apple Silicon package power: one host, three optional readings,
            // each stored only on a tick the exporter actually published it.
            agent_host("host_package_power_watts", "W", "muser_host_package_power_watts"),
            agent_host("host_cpu_power_watts", "W", "muser_host_cpu_power_watts"),
            agent_host("host_gpu_power_watts", "W", "muser_host_gpu_power_watts"),
        ]
    };
}

pub static AGENT_CATALOG: &[Series] = agent_catalog!("0", "1", "2", "3", "4", "5", "6", "7");

/// Everything the console can store: engine series first, agent series
/// after, each half in its own declared order.
pub fn all() -> impl Iterator<Item = &'static Series> {
    CATALOG.iter().chain(AGENT_CATALOG.iter())
}

/// Exact-string lookup. Series names are the console's own vocabulary, not
/// user input; an unknown name is a 400 from the query API, never a guess.
pub fn lookup(name: &str) -> Option<&'static Series> {
    all().find(|series| series.name == name)
}

pub fn with_source(source: Source) -> impl Iterator<Item = &'static Series> {
    all().filter(move |series| series.source == source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_names_are_unique() {
        let names: Vec<&str> = all().map(|series| series.name).collect();
        for (index, name) in names.iter().enumerate() {
            assert!(
                names[..index].iter().all(|other| other != name),
                "duplicate catalog series '{name}': one name, one source"
            );
        }
    }

    #[test]
    fn metrics_series_have_no_honesty_path_and_snapshot_series_do() {
        for series in all() {
            match series.source {
                // The exposition carries only measured series, so there is
                // no path to read — not an empty string, an absent path.
                Source::Metrics => assert!(
                    series.honesty_path.is_none(),
                    "{} is a /metrics series and must not claim an _honesty path",
                    series.name
                ),
                Source::Snapshot => assert!(
                    series.honesty_path.is_some(),
                    "{} is a /snapshot series and must name its _honesty path",
                    series.name
                ),
                // An agent series has no snapshot sidecar behind it at all;
                // its honesty is agent-measured, written by the sampler.
                Source::Agent => assert!(
                    series.honesty_path.is_none(),
                    "{} is an agent series and must not claim an engine _honesty path",
                    series.name
                ),
            }
        }
    }

    #[test]
    fn extraction_matches_source() {
        for series in all() {
            let consistent = matches!(
                (series.source, series.extraction),
                (Source::Metrics, Extraction::Prometheus { .. })
                    | (
                        Source::Snapshot,
                        Extraction::Snapshot { .. }
                            | Extraction::TransfersActive
                            | Extraction::TransferLast { .. },
                    )
                    | (Source::Agent, Extraction::Agent { .. })
            );
            assert!(consistent, "{} extraction/source mismatch", series.name);
        }
    }

    #[test]
    fn agent_series_name_the_device_their_label_selects() {
        // The store's key is (instance, series, ts): if a series name and the
        // `gpu` label it reads ever disagreed, two devices would collide on
        // one row. Every catalog entry is checked, not the macro's shape.
        let mut gpu_entries = 0usize;
        for series in AGENT_CATALOG {
            let Extraction::Agent { metric, gpu } = series.extraction else {
                panic!("{} must extract from an agent metric", series.name);
            };
            match gpu {
                Some(index) => {
                    gpu_entries += 1;
                    assert!(
                        series.name.starts_with(&format!("gpu{index}_")),
                        "{} reads the gpu=\"{index}\" label but does not say so in its name",
                        series.name
                    );
                    assert!(
                        metric.starts_with("muser_gpu_"),
                        "{} selects a gpu label on a non-device metric",
                        series.name
                    );
                }
                None => assert!(
                    series.name.starts_with("host_"),
                    "{} is a whole-host reading and must be named as one",
                    series.name
                ),
            }
        }
        assert_eq!(
            gpu_entries,
            MAX_AGENT_GPUS * 5,
            "five readings per device, for exactly MAX_AGENT_GPUS devices"
        );
    }

    #[test]
    fn agent_measured_is_its_own_honesty_value() {
        // The whole point of the phase: an agent number must never be able to
        // present itself as something the engine measured.
        assert_eq!(Honesty::AgentMeasured.as_str(), "agent-measured");
        assert_ne!(Honesty::AgentMeasured, Honesty::Measured);
        assert_eq!(
            Honesty::from_tag("agent-measured"),
            Some(Honesty::AgentMeasured)
        );
    }

    #[test]
    fn mock_tag_never_becomes_an_honesty_value() {
        assert_eq!(Honesty::from_tag("mock"), None);
        assert_eq!(Honesty::from_tag(""), None);
        assert_eq!(Honesty::from_tag("measured"), Some(Honesty::Measured));
        assert_eq!(Honesty::from_tag("target"), Some(Honesty::Target));
    }

    #[test]
    fn source_round_trips_through_its_stored_text() {
        for source in [Source::Metrics, Source::Snapshot, Source::Agent] {
            assert_eq!(Source::from_stored(source.as_str()), Some(source));
        }
        assert_eq!(Source::from_stored("simulated"), None);
    }
}

//! `gx10-exporter` — NVIDIA GPU telemetry the muser engine cannot see.
//!
//! The engine tags its `nodes[]` block `mock` and always serves it empty:
//! per-node GPU utilization, power, temperature and memory have no engine
//! data source at all. This exporter is that data source. It runs as a
//! sidecar process inside the node's existing pinned container, reads NVML,
//! and publishes a Prometheus exposition the console scrapes and stores
//! with `source = agent` / `honesty = agent-measured` — deliberately a
//! different badge from the engine's own `measured`.
//!
//! The honesty rules are the whole point and they are enforced structurally:
//!
//! * every reading is an `Option` from the moment it leaves NVML, and a
//!   `None` publishes nothing — no zero, no last-known value, no estimate;
//! * `muser_agent_up` is 1 only on a scrape the source actually answered,
//!   and when it is 0 the exposition carries nothing else;
//! * a metric family with no readings is omitted entirely, HELP and TYPE
//!   included, because a header with no samples announces a series the
//!   exporter does not have;
//! * the binary can only ever construct the NVML source. `RecordedSource`
//!   exists for tests and no command line can select it.

pub mod expo;
pub mod logging;
pub mod nvml;
pub mod server;
pub mod source;

pub use expo::{render, CONTENT_TYPE};
pub use nvml::NvmlSource;
pub use server::{router, ExporterState};
pub use source::{DeviceSample, GpuSource, RecordedSource, ScrapeResult};

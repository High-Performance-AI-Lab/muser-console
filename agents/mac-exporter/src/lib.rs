//! mac-exporter: Apple Silicon host power the muser engine cannot see.
//!
//! The engine's snapshot tags per-node power as `mock` because no data source
//! exists engine-side. This sidecar exposes the one the host actually has —
//! `powermetrics` — as Prometheus text the console scrapes and stores with an
//! `agent-measured` honesty tag, visibly distinct from engine-reported data.
//!
//! The honesty rules the whole project runs on apply here without exception:
//!
//! * A scrape that could not obtain a reading publishes `muser_agent_up 0`
//!   and **no** power series. Not a zero, not the last value it remembers,
//!   not an estimate. Absence is the honest report.
//! * A field `powermetrics` did not print is omitted; the fields it did print
//!   still publish.
//! * A cached reading is a real measurement and is served only inside a
//!   documented freshness window ([`exporter::MAX_READING_AGE`]). Past that it
//!   is not served at all — a stale number presented as current is a lie the
//!   scraper cannot see.
//!
//! `powermetrics` requires root. This exporter never escalates privileges: run
//! as root or get `muser_agent_up 0`, which is the true statement about a
//! process that cannot read the counters.

pub mod expo;
pub mod exporter;
pub mod host;
pub mod logging;
pub mod parse;
pub mod server;
pub mod source;

pub use exporter::{Exporter, Scrape, Served};
pub use parse::{parse_powermetrics, PowerField, Reading};
pub use server::router;
pub use source::{PowerSource, Powermetrics, RecordedSource, SourceError};

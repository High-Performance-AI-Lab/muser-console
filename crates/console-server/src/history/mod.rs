//! The history plane: sampler, rolling store, and query API.
//!
//! This is a second, separate read path. The state plane (`/snapshot`,
//! `/telemetry`, `/stream`) is untouched by anything in here: live tiles
//! keep reading the engine directly, and nothing the sampler stores is ever
//! served as if it were live. The two planes join only on the pinned field
//! names in the catalog, which is what makes "the chart and the tile agree"
//! a checkable property rather than a promise.

pub mod api;
pub mod catalog;
pub mod prom;
pub mod sampler;
pub mod store;

pub use sampler::{now_ms, spawn};
pub use store::HistoryStore;

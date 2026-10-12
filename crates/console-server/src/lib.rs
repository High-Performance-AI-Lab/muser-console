//! muser-console: credential-isolating reverse proxy for the muser engine
//! dashboard. The engine repository is read-only for this project; auth and
//! error shapes here mirror `muser-server/src/axum_httpd.rs` byte-for-byte
//! where the dashboard depends on them.

pub mod auth;
pub mod config;
pub mod history;
pub mod logging;
pub mod proxy;
pub mod routes;
pub mod state;
pub mod ws;

pub use config::Config;
pub use history::HistoryStore;
pub use routes::router;
pub use state::AppState;

//! Minimal stderr logging, matching the console server's. The exporter holds
//! no credentials, but the same rule applies: nothing that arrived from a
//! network peer is ever logged.

pub fn log(message: &str) {
    eprintln!("mac-exporter: {message}");
}

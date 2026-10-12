//! Minimal stderr logging, mirroring the console server's.
//!
//! This exporter holds no credentials and is never given one, so there is
//! nothing secret to leak here — but the rule stands anyway: never pass a
//! header value or key material to `log`.

pub fn log(message: &str) {
    eprintln!("gx10-exporter: {message}");
}

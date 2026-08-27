//! Minimal stderr logging. Never pass header values or key material here.

pub fn log(message: &str) {
    eprintln!("muser-console: {message}");
}

/// A bounded source chain for transport failures. Error objects from hyper,
/// rustls, and the socket layer contain protocol/certificate causes but not
/// request headers or bodies. Keeping this helper here makes the boundary
/// explicit and avoids switching on broad debug formatting at call sites.
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    for _ in 0..4 {
        let Some(cause) = source else { break };
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

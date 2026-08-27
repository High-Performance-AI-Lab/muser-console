use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use console_server::logging::log;
use console_server::{history, router, AppState, Config, HistoryStore};

const USAGE: &str = "usage: muser-console [--config <path>] [--listen <addr>]";

/// How long graceful shutdown waits for in-flight responses. Proxied SSE
/// streams never end on their own, so without this bound Ctrl-C would hang
/// forever while any /telemetry or progress stream is open. Mirrors the
/// engine's 5 s shutdown grace.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let (config_path, listen_override) = match parse_arguments(&arguments) {
        Ok(parsed) => parsed,
        Err(error) => {
            log(&error);
            log(USAGE);
            return ExitCode::from(2);
        }
    };

    let config = match Config::load(&config_path, listen_override.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            log(&error);
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log(&format!("start runtime: {error}"));
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(serve(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log(&error);
            ExitCode::FAILURE
        }
    }
}

async fn serve(config: Config) -> Result<(), String> {
    let listen = config.listen;
    let default_name = config.instances[0].name.clone();
    let default_base = config.instances[0].base_url.clone();
    let tls_material = config
        .tls
        .as_ref()
        .map(|tls| (tls.cert_pem.clone(), tls.key_pem.clone()));

    // The history plane is opened before the router so a bad store path
    // fails at startup rather than at the first chart request.
    let state = if config.history.enabled {
        let store = HistoryStore::open(&config.history)?;
        log(&format!(
            "history plane: {} ({} s sampling, {} day retention)",
            store.path().display(),
            config.history.sample_interval_ms as f64 / 1000.0,
            config.history.retention_days
        ));
        AppState::with_history(config, store)
    } else {
        log("history plane: disabled by config; charts will report it unavailable");
        AppState::new(config)
    };
    // Samplers run for the process lifetime; they hold their own state
    // handles and stop when the process does.
    let _workers = history::spawn(&state);

    let application = router(state);
    if let Some((cert_pem, key_pem)) = tls_material {
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem)
            .await
            .map_err(|error| format!("load console TLS certificate/key: {error}"))?;
        let listener = bind_nonblocking_listener(listen)?;
        let bound = listener
            .local_addr()
            .map_err(|error| format!("local addr: {error}"))?;
        log(&format!(
            "listening on https://{bound} (default instance '{default_name}' -> {default_base})"
        ));
        let handle = axum_server::Handle::new();
        let shutdown = handle.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.graceful_shutdown(Some(SHUTDOWN_GRACE));
        });
        return axum_server::from_tcp_rustls(listener, tls)
            .map_err(|error| format!("prepare TLS listener: {error}"))?
            .handle(handle)
            .serve(application.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
            .map_err(|error| format!("serve TLS: {error}"));
    }

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| format!("bind {listen}: {error}"))?;
    let bound = listener
        .local_addr()
        .map_err(|error| format!("local addr: {error}"))?;
    log(&format!(
        "listening on http://{bound} (default instance '{default_name}' -> {default_base})"
    ));
    let (signal_seen_tx, signal_seen_rx) = tokio::sync::oneshot::channel::<()>();
    let graceful = axum::serve(
        listener,
        application.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = signal_seen_tx.send(());
    });
    tokio::select! {
        result = graceful => result.map_err(|error| format!("serve: {error}")),
        _ = async {
            let _ = signal_seen_rx.await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => {
            log("shutdown grace elapsed with streams still open; exiting");
            Ok(())
        }
    }
}

fn bind_nonblocking_listener(
    listen: std::net::SocketAddr,
) -> Result<std::net::TcpListener, String> {
    let listener =
        std::net::TcpListener::bind(listen).map_err(|error| format!("bind {listen}: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set {listen} nonblocking: {error}"))?;
    Ok(listener)
}

fn parse_arguments(arguments: &[String]) -> Result<(PathBuf, Option<String>), String> {
    let mut config = PathBuf::from("console.toml");
    let mut listen = None;
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--config" => {
                config = PathBuf::from(iter.next().ok_or("--config requires a path")?);
            }
            "--listen" => {
                listen = Some(iter.next().ok_or("--listen requires an address")?.clone());
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok((config, listen))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_listener_is_nonblocking_before_tokio_registration() {
        let listener = bind_nonblocking_listener("127.0.0.1:0".parse().unwrap()).unwrap();
        let error = listener.accept().expect_err("no connection is pending");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }
}

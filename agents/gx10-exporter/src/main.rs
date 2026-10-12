use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use gx10_exporter::logging::log;
use gx10_exporter::{router, ExporterState, NvmlSource};

const USAGE: &str = "usage: gx10-exporter [--listen <addr>]";

/// Loopback by default. The console usually scrapes across the network, so
/// a wider bind is allowed — but it has to be asked for explicitly, and it
/// is logged for what it is.
const DEFAULT_LISTEN: &str = "127.0.0.1:9707";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let listen = match parse_arguments(&arguments) {
        Ok(listen) => listen,
        Err(error) => {
            log(&error);
            log(USAGE);
            return ExitCode::from(2);
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

    match runtime.block_on(serve(listen)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log(&error);
            ExitCode::FAILURE
        }
    }
}

async fn serve(listen: SocketAddr) -> Result<(), String> {
    // The only source this binary can construct. There is no flag, config
    // key, or environment variable that makes the process publish anything
    // but what NVML answered.
    let state = ExporterState::new(Arc::new(NvmlSource::new()));

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| format!("bind {listen}: {error}"))?;
    let bound = listener
        .local_addr()
        .map_err(|error| format!("local addr: {error}"))?;
    log(&format!(
        "listening on http://{bound} (GET /metrics, GET /healthz)"
    ));
    if !bound.ip().is_loopback() {
        log(
            "bind is not loopback: this exporter has no authentication and holds no \
             credentials, so anyone who can reach it can read the node's GPU counters. \
             Restrict it at the network layer.",
        );
    }
    log(
        "NVML is opened at scrape time; until it answers, /metrics reports \
         muser_agent_up 0 and no device series",
    );

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|error| format!("serve: {error}"))
}

fn parse_arguments(arguments: &[String]) -> Result<SocketAddr, String> {
    let mut listen = DEFAULT_LISTEN.to_owned();
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--listen" => {
                listen = iter.next().ok_or("--listen requires an address")?.clone();
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    listen
        .parse()
        .map_err(|_| format!("--listen '{listen}' is not a host:port address"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn the_default_bind_is_loopback() {
        let listen = parse_arguments(&[]).expect("defaults parse");
        assert_eq!(listen, DEFAULT_LISTEN.parse::<SocketAddr>().expect("addr"));
        assert!(listen.ip().is_loopback());
        assert_eq!(listen.port(), 9707);
    }

    #[test]
    fn a_wider_bind_has_to_be_spelled_out() {
        let listen = parse_arguments(&arguments(&["--listen", "0.0.0.0:9707"])).expect("parses");
        assert!(!listen.ip().is_loopback());
    }

    #[test]
    fn bad_arguments_are_refused_rather_than_guessed_at() {
        assert!(parse_arguments(&arguments(&["--listen"])).is_err());
        assert!(parse_arguments(&arguments(&["--listen", "not-an-address"])).is_err());
        assert!(parse_arguments(&arguments(&["--api-key-file", "k"])).is_err());
    }
}

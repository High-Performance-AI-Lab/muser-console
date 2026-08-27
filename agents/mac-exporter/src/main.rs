use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use mac_exporter::exporter::{Exporter, MAX_READING_AGE};
use mac_exporter::logging::log;
use mac_exporter::source::{PowerSource, Powermetrics};
use mac_exporter::{host, server};

const USAGE: &str = "usage: mac-exporter [--listen <addr>] [--sample-ms <ms>]";

/// Loopback by default. The console may scrape across the network, so a wider
/// bind is allowed — but only as an explicit `--listen`, and the exporter says
/// so out loud when it happens.
const DEFAULT_LISTEN: &str = "127.0.0.1:9708";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_arguments(&arguments) {
        Ok(options) => options,
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

    match runtime.block_on(serve(options)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log(&error);
            ExitCode::FAILURE
        }
    }
}

struct Options {
    listen: SocketAddr,
    sample_ms: u64,
}

async fn serve(options: Options) -> Result<(), String> {
    let host = host::detect();
    if host.is_none() {
        log("hostname unreadable or unusable as a label; power series will publish without a host label");
    }
    let exporter = Arc::new(Exporter::new(
        PowerSource::powermetrics(options.sample_ms),
        host,
    ));

    let listener = tokio::net::TcpListener::bind(options.listen)
        .await
        .map_err(|error| format!("bind {}: {error}", options.listen))?;
    let bound = listener
        .local_addr()
        .map_err(|error| format!("local addr: {error}"))?;
    log(&format!(
        "listening on http://{bound} (persistent powermetrics -i {} ms, readings served for at most {:.1} s)",
        options.sample_ms,
        MAX_READING_AGE.as_secs_f64()
    ));
    if !bound.ip().is_loopback() {
        log(
            "this bind is not loopback: the exporter has no authentication because it holds no \
             secrets and serves only host power numbers. Do not send it a credential.",
        );
    }
    log("powermetrics requires root; this exporter never escalates. Without it every scrape reports muser_agent_up 0 and publishes no power series.");

    server::serve(listener, exporter, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(|error| format!("serve: {error}"))
}

fn parse_arguments(arguments: &[String]) -> Result<Options, String> {
    let mut listen = DEFAULT_LISTEN.to_owned();
    let mut sample_ms = Powermetrics::DEFAULT_SAMPLE_MS;
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--listen" => {
                listen = iter.next().ok_or("--listen requires an address")?.clone();
            }
            "--sample-ms" => {
                let raw = iter.next().ok_or("--sample-ms requires a value")?;
                sample_ms = raw
                    .parse()
                    .map_err(|_| format!("--sample-ms must be a positive integer, got '{raw}'"))?;
                if sample_ms == 0 {
                    return Err("--sample-ms must be at least 1".to_owned());
                }
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    let listen: SocketAddr = listen
        .parse()
        .map_err(|_| format!("--listen must be an ip:port address, got '{listen}'"))?;
    Ok(Options { listen, sample_ms })
}

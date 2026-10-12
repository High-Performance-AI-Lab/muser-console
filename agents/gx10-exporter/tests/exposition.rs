//! End-to-end checks of what the exporter actually puts on the wire.
//!
//! The readings below are **structural stimulus, not measurements**. No
//! machine in this project has an NVIDIA GPU, so there is no NVML capture
//! to replay and none is invented: these values exist only to drive the
//! renderer's branches (a full device, a device with two failed probes, a
//! driver string that needs escaping) and no number here is presented
//! anywhere as telemetry. The one thing the tests assert about values is
//! that a value the source did not produce never appears at all.
//!
//! The HTTP client is a raw socket on purpose — one more dependency in the
//! tree to send four lines of HTTP/1.1 would not be worth it.

use std::net::SocketAddr;
use std::sync::Arc;

use gx10_exporter::{router, DeviceSample, ExporterState, RecordedSource, CONTENT_TYPE};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// Harness

async fn spawn(source: RecordedSource) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let address = listener.local_addr().expect("local addr");
    let application = router(ExporterState::new(Arc::new(source)));
    tokio::spawn(async move {
        let _ = axum::serve(listener, application).await;
    });
    address
}

/// One HTTP/1.1 GET with `Connection: close`, returning the head block and
/// the body exactly as they came off the socket.
async fn get(address: SocketAddr, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(address).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    let text = String::from_utf8(raw).expect("response is utf-8");
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("response has a header block");
    (head.to_owned(), body.to_owned())
}

// ---------------------------------------------------------------------------
// A structural parser for the exposition, so "well-formed" is checked
// rather than eyeballed: one HELP and one TYPE per family, samples grouped
// under their own family, every line a readable `name{labels} value`.

#[derive(Debug)]
struct Family {
    name: String,
    help: String,
    kind: String,
    samples: Vec<Sample>,
}

#[derive(Debug)]
struct Sample {
    labels: String,
    value: f64,
}

fn parse_exposition(text: &str) -> Vec<Family> {
    let mut families: Vec<Family> = Vec::new();
    for line in text.lines() {
        assert!(!line.trim().is_empty(), "no blank lines in the exposition");
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let (name, help) = rest.split_once(' ').expect("HELP names and describes");
            assert!(
                families.iter().all(|family| family.name != name),
                "one HELP per family: {name}"
            );
            families.push(Family {
                name: name.to_owned(),
                help: help.to_owned(),
                kind: String::new(),
                samples: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest.split_once(' ').expect("TYPE names and types");
            let family = families.last_mut().expect("TYPE follows its own HELP");
            assert_eq!(family.name, name, "TYPE follows its own family's HELP");
            assert!(family.kind.is_empty(), "one TYPE per family: {name}");
            family.kind = kind.to_owned();
        } else {
            assert!(
                !line.starts_with('#'),
                "only HELP and TYPE comments: {line}"
            );
            let (name, labels, value) = split_sample(line);
            let family = families
                .last_mut()
                .expect("a sample follows its family header");
            assert_eq!(family.name, name, "samples stay under their own family");
            assert!(
                !family.kind.is_empty(),
                "a family declares its TYPE before its samples: {name}"
            );
            family.samples.push(Sample {
                labels,
                value: value.parse().unwrap_or_else(|_| {
                    panic!("sample value must be a number, got '{value}' in: {line}")
                }),
            });
        }
    }
    families
}

fn split_sample(line: &str) -> (String, String, String) {
    match line.split_once('{') {
        Some((name, rest)) => {
            let (labels, value) = rest
                .rsplit_once("} ")
                .expect("the label set closes before the value");
            (name.to_owned(), labels.to_owned(), value.to_owned())
        }
        None => {
            let (name, value) = line.split_once(' ').expect("a bare sample is 'name value'");
            (name.to_owned(), String::new(), value.to_owned())
        }
    }
}

fn family<'a>(families: &'a [Family], name: &str) -> Option<&'a Family> {
    families.iter().find(|family| family.name == name)
}

fn require<'a>(families: &'a [Family], name: &str) -> &'a Family {
    family(families, name).unwrap_or_else(|| panic!("{name} must be published"))
}

// ---------------------------------------------------------------------------
// Stimulus (see the module note: shapes, not measurements)

fn device_that_answered_everything() -> DeviceSample {
    DeviceSample {
        index: 0,
        uuid: Some("GPU-11111111-2222-3333-4444-555555555555".to_owned()),
        name: Some("test-device".to_owned()),
        utilization_ratio: Some(0.5),
        power_watts: Some(12.5),
        temperature_celsius: Some(40.0),
        memory_used_bytes: Some(1024),
        memory_total_bytes: Some(4096),
    }
}

/// A second device whose power and temperature probes failed.
fn device_with_two_failed_probes() -> DeviceSample {
    DeviceSample {
        index: 1,
        uuid: Some("GPU-66666666-7777-8888-9999-aaaaaaaaaaaa".to_owned()),
        name: Some("test-device".to_owned()),
        utilization_ratio: Some(0.25),
        power_watts: None,
        temperature_celsius: None,
        memory_used_bytes: Some(2048),
        memory_total_bytes: Some(4096),
    }
}

// ---------------------------------------------------------------------------
// Tests

#[tokio::test]
async fn an_answered_scrape_serves_a_well_formed_exposition() {
    let address = spawn(RecordedSource::devices(vec![
        device_that_answered_everything(),
        device_with_two_failed_probes(),
    ]))
    .await;
    let (head, body) = get(address, "/metrics").await;

    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains(&format!("content-type: {CONTENT_TYPE}\r\n").to_ascii_lowercase()),
        "the exposition format version is part of the contract: {head}"
    );

    let families = parse_exposition(&body);
    let names: Vec<&str> = families.iter().map(|family| family.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "muser_agent_up",
            "muser_agent_scrape_duration_seconds",
            "muser_gpu_utilization_ratio",
            "muser_gpu_power_watts",
            "muser_gpu_temperature_celsius",
            "muser_gpu_memory_used_bytes",
            "muser_gpu_memory_total_bytes",
        ]
    );
    for entry in &families {
        assert_eq!(entry.kind, "gauge");
        assert!(!entry.help.is_empty(), "{} needs HELP", entry.name);
        assert!(!entry.samples.is_empty(), "{} has samples", entry.name);
    }

    let up = require(&families, "muser_agent_up");
    assert_eq!(up.samples.len(), 1);
    assert_eq!(up.samples[0].labels, "agent=\"gx10\"");
    assert_eq!(up.samples[0].value, 1.0);

    let duration = require(&families, "muser_agent_scrape_duration_seconds");
    assert_eq!(duration.samples[0].labels, "agent=\"gx10\"");
    assert!(
        duration.samples[0].value.is_finite() && duration.samples[0].value >= 0.0,
        "a scrape duration is a real elapsed time"
    );

    let utilization = require(&families, "muser_gpu_utilization_ratio");
    assert_eq!(utilization.samples.len(), 2, "both devices reported it");
    assert_eq!(
        utilization.samples[0].labels,
        "gpu=\"0\",uuid=\"GPU-11111111-2222-3333-4444-555555555555\",name=\"test-device\""
    );
    assert_eq!(
        utilization.samples[1].labels,
        "gpu=\"1\",uuid=\"GPU-66666666-7777-8888-9999-aaaaaaaaaaaa\",name=\"test-device\""
    );
    assert!(
        utilization
            .samples
            .iter()
            .all(|sample| (0.0..=1.0).contains(&sample.value)),
        "NVML percent is published as a ratio"
    );

    // The HELP has to say where the ratio came from — a number that quietly
    // changed units is the kind of thing this project exists to prevent.
    assert!(utilization.help.contains("percent"), "{}", utilization.help);
    assert!(require(&families, "muser_gpu_power_watts")
        .help
        .contains("milliwatts"));
}

#[tokio::test]
async fn a_failed_probe_omits_only_that_field() {
    let address = spawn(RecordedSource::devices(vec![
        device_that_answered_everything(),
        device_with_two_failed_probes(),
    ]))
    .await;
    let (_, body) = get(address, "/metrics").await;
    let families = parse_exposition(&body);

    for name in ["muser_gpu_power_watts", "muser_gpu_temperature_celsius"] {
        let entry = require(&families, name);
        assert_eq!(
            entry.samples.len(),
            1,
            "{name} only for the device that answered"
        );
        assert!(
            entry.samples[0].labels.starts_with("gpu=\"0\","),
            "{name} belongs to gpu 0"
        );
    }
    assert!(
        !body.contains("gpu=\"1\"} 0") && !body.contains("muser_gpu_power_watts{gpu=\"1\""),
        "a probe that failed publishes nothing, least of all a zero:\n{body}"
    );

    // Everything gpu 1 did report is still there.
    for name in [
        "muser_gpu_utilization_ratio",
        "muser_gpu_memory_used_bytes",
        "muser_gpu_memory_total_bytes",
    ] {
        assert_eq!(require(&families, name).samples.len(), 2, "{name}");
    }
}

#[tokio::test]
async fn a_source_that_did_not_answer_serves_only_agent_up_zero() {
    let address = spawn(RecordedSource::failing(
        "dlopen libnvidia-ml.so.1: not loadable on this host",
    ))
    .await;
    let (head, body) = get(address, "/metrics").await;

    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    let families = parse_exposition(&body);
    assert_eq!(families.len(), 1, "one family only:\n{body}");
    assert_eq!(families[0].name, "muser_agent_up");
    assert_eq!(families[0].kind, "gauge");
    assert_eq!(families[0].samples.len(), 1);
    assert_eq!(families[0].samples[0].labels, "agent=\"gx10\"");
    assert_eq!(families[0].samples[0].value, 0.0);

    assert!(
        !body.contains("muser_gpu_"),
        "no device series without a source:\n{body}"
    );
    assert!(
        !body.contains("muser_agent_scrape_duration_seconds"),
        "a failed probe's duration describes nothing about the node:\n{body}"
    );
    assert!(
        !body.contains("libnvidia-ml"),
        "the reason goes to the log, not the response body:\n{body}"
    );
}

#[tokio::test]
async fn a_working_source_with_no_gpus_is_up_with_no_device_series() {
    let address = spawn(RecordedSource::devices(Vec::new())).await;
    let (_, body) = get(address, "/metrics").await;
    let families = parse_exposition(&body);

    let names: Vec<&str> = families.iter().map(|family| family.name.as_str()).collect();
    assert_eq!(
        names,
        ["muser_agent_up", "muser_agent_scrape_duration_seconds"],
        "NVML answered and found nothing — different fact from NVML not answering"
    );
    assert_eq!(require(&families, "muser_agent_up").samples[0].value, 1.0);
}

#[tokio::test]
async fn driver_strings_are_escaped_on_the_wire() {
    // Device names come from the driver, so they are escaped rather than
    // trusted. A quote would end the label set early and a newline would
    // split the line into two bogus samples.
    let device = DeviceSample {
        index: 0,
        uuid: Some("GPU-back\\slash".to_owned()),
        name: Some("NVIDIA \"GB10\"\nrev\\2".to_owned()),
        utilization_ratio: Some(0.75),
        ..DeviceSample::default()
    };
    let address = spawn(RecordedSource::devices(vec![device])).await;
    let (_, body) = get(address, "/metrics").await;

    assert!(
        body.contains(
            "muser_gpu_utilization_ratio{gpu=\"0\",uuid=\"GPU-back\\\\slash\",\
             name=\"NVIDIA \\\"GB10\\\"\\nrev\\\\2\"} 0.75\n"
        ),
        "{body}"
    );
    // And the document is still well-formed after that round trip.
    let families = parse_exposition(&body);
    assert_eq!(
        require(&families, "muser_gpu_utilization_ratio")
            .samples
            .len(),
        1
    );
}

#[tokio::test]
async fn a_device_whose_label_probes_failed_still_publishes_its_readings() {
    let device = DeviceSample {
        index: 3,
        uuid: None,
        name: None,
        utilization_ratio: Some(0.1),
        memory_used_bytes: Some(512),
        ..DeviceSample::default()
    };
    let address = spawn(RecordedSource::devices(vec![device])).await;
    let (_, body) = get(address, "/metrics").await;
    let families = parse_exposition(&body);

    assert_eq!(
        require(&families, "muser_gpu_utilization_ratio").samples[0].labels,
        "gpu=\"3\"",
        "a label whose probe failed is absent, not empty and not invented"
    );
    assert!(
        !body.contains("uuid=\"\"") && !body.contains("name=\"\""),
        "{body}"
    );
    assert!(
        family(&families, "muser_gpu_memory_total_bytes").is_none(),
        "a family no device reported is absent, HELP and TYPE included:\n{body}"
    );
}

#[tokio::test]
async fn healthz_answers_even_while_the_gpu_source_is_down() {
    let address = spawn(RecordedSource::failing("NVML absent")).await;
    let (head, body) = get(address, "/healthz").await;

    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert_eq!(body, "{\"ok\":true}");
    assert!(
        !body.contains("NVML absent"),
        "the health route reports this process, not the hardware"
    );
}

#[tokio::test]
async fn there_is_no_route_beyond_metrics_and_healthz() {
    let address = spawn(RecordedSource::devices(vec![
        device_that_answered_everything(),
    ]))
    .await;
    for path in ["/", "/snapshot", "/v1/nodes", "/metrics/../healthz"] {
        let (head, _) = get(address, path).await;
        assert!(
            head.starts_with("HTTP/1.1 404 "),
            "{path} is not a route on this exporter: {head}"
        );
    }
}

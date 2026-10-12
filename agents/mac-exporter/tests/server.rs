//! The HTTP surface, over a real socket.
//!
//! The client here is twenty lines of HTTP/1.1 rather than a dependency: the
//! exporter's surface is two GETs, and `Connection: close` makes reading a
//! response a read-to-end.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mac_exporter::expo;
use mac_exporter::exporter::Exporter;
use mac_exporter::server;
use mac_exporter::source::{PowerSource, RecordedSource};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

async fn spawn(exporter: Arc<Exporter>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local address");
    tokio::spawn(async move {
        let _ = server::serve(listener, exporter, std::future::pending::<()>()).await;
    });
    address
}

/// Returns (status line + headers, body).
async fn get(address: SocketAddr, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(address).await.expect("connect");
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: exporter.test\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read response");
    let text = String::from_utf8(raw).expect("responses are UTF-8");
    let (head, body) = text.split_once("\r\n\r\n").expect("headers end");
    (head.to_owned(), body.to_owned())
}

fn exporter(source: PowerSource) -> Arc<Exporter> {
    Arc::new(Exporter::with_max_age(
        source,
        Some("studio.local".to_owned()),
        Duration::from_secs(2),
    ))
}

#[tokio::test]
async fn metrics_serves_prometheus_text_with_the_pinned_content_type() {
    let address = spawn(exporter(PowerSource::recorded(RecordedSource::text(
        "CPU Power: 1234 mW\nGPU Power: 56.25 mW\n",
    ))))
    .await;
    let (head, body) = get(address, "/metrics").await;

    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains(&format!("content-type: {}", expo::CONTENT_TYPE)),
        "{head}"
    );
    assert!(body.contains("muser_agent_up{agent=\"mac\"} 1"), "{body}");
    // The source line is a label too, so the host label is no longer the
    // whole label set — match the parts that carry meaning.
    assert!(
        body.contains(
            "muser_host_cpu_power_watts{host=\"studio.local\",source=\"CPU Power\"} 1.234"
        ),
        "{body}"
    );
    assert!(
        !body.contains("muser_host_package_power_watts"),
        "the input carried no package line: {body}"
    );
}

#[tokio::test]
async fn metrics_of_an_unreadable_source_is_a_valid_exposition_with_no_power_series() {
    let address = spawn(exporter(PowerSource::recorded(RecordedSource::not_root()))).await;
    let (head, body) = get(address, "/metrics").await;

    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(body.contains("muser_agent_up{agent=\"mac\"} 0"), "{body}");
    assert!(!body.contains("muser_host_"), "{body}");
}

#[tokio::test]
async fn healthz_reports_the_process_and_nothing_more() {
    let address = spawn(exporter(PowerSource::recorded(RecordedSource::not_root()))).await;
    let (head, body) = get(address, "/healthz").await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(body, "{\"ok\":true}");
}

#[tokio::test]
async fn an_unknown_path_is_a_plain_404() {
    let address = spawn(exporter(PowerSource::recorded(RecordedSource::not_root()))).await;
    let (head, body) = get(address, "/v1/nodes").await;
    assert!(head.starts_with("HTTP/1.1 404 Not Found"), "{head}");
    assert!(body.is_empty(), "{body}");
}

/// The real command, on the machine running the tests. Nothing here asserts a
/// power value: it asserts that whichever way `powermetrics` answers, the
/// exposition is honest about it. Without root — the state every development
/// machine is in — that means `muser_agent_up 0` and no power series.
#[cfg(all(target_os = "macos", feature = "real-powermetrics-tests"))]
#[tokio::test]
async fn the_real_powermetrics_path_reports_what_it_actually_got() {
    let exporter = Arc::new(Exporter::new(
        PowerSource::powermetrics(50),
        Some("studio.local".to_owned()),
    ));
    let address = spawn(exporter).await;
    let (head, body) = get(address, "/metrics").await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");

    if body.contains("muser_agent_up{agent=\"mac\"} 0") {
        assert!(
            !body.contains("muser_host_"),
            "a scrape that got nothing publishes nothing: {body}"
        );
    } else {
        // Running as root: powermetrics answered, so the shape of a real
        // reading is checked instead. The values are whatever the tool said.
        assert!(body.contains("muser_agent_up{agent=\"mac\"} 1"), "{body}");
        assert!(
            body.contains("# powermetrics sample completed at unix "),
            "{body}"
        );
    }
}

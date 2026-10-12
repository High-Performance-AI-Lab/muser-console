//! What the exporter publishes, and — more to the point — what it refuses to.
//!
//! The readings here come from the crate's structure fixture and from inline
//! parser inputs; no assertion treats any of those numbers as a measurement of
//! hardware. What is asserted is the exposition's honesty behaviour: absent
//! stays absent, stale is never served, and a failed source publishes nothing
//! but its own down signal.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mac_exporter::expo;
use mac_exporter::exporter::Exporter;
use mac_exporter::source::{PowerSource, RecordedSource, SourceError};

fn fixture() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/powermetrics-cpu-gpu-power.structure.txt");
    std::fs::read_to_string(path).expect("structure fixture must be readable")
}

fn exporter(source: PowerSource, host: Option<&str>, max_age: Duration) -> Arc<Exporter> {
    Arc::new(Exporter::with_max_age(
        source,
        host.map(str::to_owned),
        max_age,
    ))
}

async fn exposition(exporter: &Exporter) -> String {
    let scrape = exporter.scrape().await;
    expo::render(&scrape, exporter.host(), exporter.max_age())
}

/// Every non-comment, non-blank line of an exposition, as (series, value).
fn samples(body: &str) -> Vec<(String, String)> {
    body.lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (series, value) = line
                .rsplit_once(' ')
                .unwrap_or_else(|| panic!("exposition line has no value: {line}"));
            value
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("exposition value is not a number: {line}"));
            (series.to_owned(), value.to_owned())
        })
        .collect()
}

/// Looks a value up by metric name, or by the exact `name{labels}` string
/// when the caller spells labels out.
fn value_of(body: &str, series: &str) -> Option<String> {
    samples(body)
        .into_iter()
        .find(|(name, _)| {
            if series.contains('{') {
                name == series
            } else {
                name.split('{').next() == Some(series)
            }
        })
        .map(|(_, value)| value)
}

fn metric_names(body: &str) -> Vec<String> {
    samples(body)
        .into_iter()
        .map(|(series, _)| {
            series
                .split_once('{')
                .map(|(name, _)| name.to_owned())
                .unwrap_or(series)
        })
        .collect()
}

#[tokio::test]
async fn a_reading_publishes_every_field_it_has_with_help_and_type() {
    let exporter = exporter(
        PowerSource::recorded(RecordedSource::text(&fixture())),
        Some("studio.local"),
        Duration::from_secs(2),
    );
    let body = exposition(&exporter).await;

    assert_eq!(
        value_of(&body, "muser_agent_up{agent=\"mac\"}").as_deref(),
        Some("1")
    );
    assert_eq!(
        value_of(&body, "muser_host_package_power_watts").as_deref(),
        Some("1.2905"),
        "the value the fixture's Package Power line carries, in watts"
    );
    assert_eq!(
        value_of(&body, "muser_host_cpu_power_watts").as_deref(),
        Some("1.234")
    );
    assert_eq!(
        value_of(&body, "muser_host_gpu_power_watts").as_deref(),
        Some("0.05625")
    );

    for metric in metric_names(&body) {
        assert!(
            body.contains(&format!("# HELP {metric} ")),
            "{metric} must carry HELP"
        );
        assert!(
            body.contains(&format!("# TYPE {metric} gauge")),
            "{metric} must carry TYPE"
        );
    }
    assert!(
        body.contains("is read from the powermetrics line \"Package Power\"."),
        "the exposition names the line each number came from"
    );
    assert!(
        body.contains("# powermetrics sample completed at unix "),
        "the sample's own timestamp is a comment, not a series"
    );
    assert!(
        body.ends_with('\n'),
        "every exposition line is newline-terminated"
    );
}

#[tokio::test]
async fn a_source_that_cannot_be_read_publishes_only_agent_up_zero() {
    // The overwhelmingly common case: powermetrics without root.
    let exporter = exporter(
        PowerSource::recorded(RecordedSource::not_root()),
        Some("studio.local"),
        Duration::from_secs(2),
    );
    let body = exposition(&exporter).await;

    assert_eq!(
        value_of(&body, "muser_agent_up{agent=\"mac\"}").as_deref(),
        Some("0")
    );
    assert_eq!(
        metric_names(&body),
        vec!["muser_agent_up".to_owned()],
        "no power series at all — not a zero, not a placeholder"
    );
    assert!(
        !body.contains("superuser"),
        "the child's stderr goes to the log, never into a response body"
    );
    assert!(
        body.contains("# no reading this scrape: the powermetrics process exited non-zero"),
        "the exposition says why, in a phrase the exporter wrote itself"
    );
}

#[tokio::test]
async fn a_spawn_failure_publishes_only_agent_up_zero() {
    let exporter = exporter(
        PowerSource::recorded(RecordedSource::new(vec![Err(SourceError::Spawn(
            "No such file or directory (os error 2)".to_owned(),
        ))])),
        None,
        Duration::from_secs(2),
    );
    let body = exposition(&exporter).await;
    assert_eq!(
        value_of(&body, "muser_agent_up{agent=\"mac\"}").as_deref(),
        Some("0")
    );
    assert!(!body.contains("muser_host_"), "no power series");
    assert!(
        !body.contains("os error 2"),
        "the spawn error's detail stays in the log"
    );
}

#[tokio::test]
async fn a_field_the_output_omits_publishes_nothing_while_the_others_publish() {
    let exporter = exporter(
        PowerSource::recorded(RecordedSource::text("CPU Power: 1234 mW\n")),
        Some("studio.local"),
        Duration::from_secs(2),
    );
    let body = exposition(&exporter).await;

    assert_eq!(
        value_of(&body, "muser_host_cpu_power_watts").as_deref(),
        Some("1.234")
    );
    assert!(
        !body.contains("muser_host_package_power_watts"),
        "an absent field takes its HELP and TYPE with it"
    );
    assert!(!body.contains("muser_host_gpu_power_watts"));
    assert_eq!(
        value_of(&body, "muser_agent_up{agent=\"mac\"}").as_deref(),
        Some("1"),
        "the source answered; only one of its fields was there"
    );
}

#[tokio::test]
async fn output_with_no_recognized_power_line_reports_up_and_publishes_no_series() {
    // The distinction the whole project turns on: "the data source answered"
    // and "the data source has this field" are different facts, reported
    // separately.
    let exporter = exporter(
        PowerSource::recorded(RecordedSource::text("**** Processor usage ****\n")),
        None,
        Duration::from_secs(2),
    );
    let body = exposition(&exporter).await;
    assert_eq!(
        value_of(&body, "muser_agent_up{agent=\"mac\"}").as_deref(),
        Some("1")
    );
    assert!(!body.contains("muser_host_"), "no fields, no series");
}

#[tokio::test]
async fn a_reading_is_reused_inside_the_window_instead_of_re_running_powermetrics() {
    let exporter = exporter(
        PowerSource::recorded(RecordedSource::text(&fixture())),
        None,
        Duration::from_secs(60),
    );
    let first = exposition(&exporter).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let second = exposition(&exporter).await;

    let calls = exporter
        .source()
        .recorded_source()
        .expect("recorded source")
        .calls();
    assert_eq!(calls, 1, "the expensive source ran once for two scrapes");
    assert_eq!(
        value_of(&first, "muser_host_package_power_watts").as_deref(),
        value_of(&second, "muser_host_package_power_watts").as_deref(),
        "the same real reading, served twice"
    );
    assert!(
        second.contains("# powermetrics sample completed at unix "),
        "the reused reading still carries the time it was taken"
    );
    assert_eq!(
        value_of(&second, "muser_agent_up{agent=\"mac\"}").as_deref(),
        Some("1")
    );
}

#[tokio::test]
async fn an_expired_reading_is_never_served_even_when_the_re_run_fails() {
    // Zero window: the first reading is stale by the second scrape, and the
    // source has stopped answering. The stale number must not reappear.
    let exporter = exporter(
        PowerSource::recorded(RecordedSource::new(vec![
            Ok(fixture()),
            Err(SourceError::Exit {
                status: "exit status: 1".to_owned(),
                stderr: "powermetrics must be invoked as the superuser".to_owned(),
            }),
        ])),
        Some("studio.local"),
        Duration::ZERO,
    );

    let first = exposition(&exporter).await;
    assert_eq!(
        value_of(&first, "muser_host_package_power_watts").as_deref(),
        Some("1.2905")
    );

    tokio::time::sleep(Duration::from_millis(5)).await;
    let second = exposition(&exporter).await;
    assert_eq!(
        value_of(&second, "muser_agent_up{agent=\"mac\"}").as_deref(),
        Some("0")
    );
    assert!(
        !second.contains("1.2905"),
        "an expired reading is a gap, not a value to repeat"
    );
    assert!(!second.contains("muser_host_"), "no power series at all");
    assert_eq!(
        exporter
            .source()
            .recorded_source()
            .expect("recorded source")
            .calls(),
        2,
        "the expired window forced a real re-measure attempt"
    );
}

#[tokio::test]
async fn label_values_are_escaped_and_an_unknown_host_carries_no_label() {
    let odd = exporter(
        PowerSource::recorded(RecordedSource::text("CPU Power: 1000 mW\n")),
        Some("we\"ird\\host"),
        Duration::from_secs(2),
    );
    let body = exposition(&odd).await;
    assert!(
        body.contains("muser_host_cpu_power_watts{host=\"we\\\"ird\\\\host\","),
        "quote and backslash are escaped, per the exposition escape set: {body}"
    );

    let nameless = exporter(
        PowerSource::recorded(RecordedSource::text("CPU Power: 1000 mW\n")),
        None,
        Duration::from_secs(2),
    );
    let body = exposition(&nameless).await;
    assert!(
        body.contains("muser_host_cpu_power_watts{source="),
        "the source line is always named: {body}"
    );
    assert!(
        !body.contains("host="),
        "no host label rather than an invented one: {body}"
    );
    assert_eq!(
        value_of(&body, "muser_host_cpu_power_watts").as_deref(),
        Some("1")
    );
}

#[tokio::test]
async fn the_reason_is_logged_once_per_state_change_not_once_per_scrape() {
    // A machine that is simply not root would otherwise write one line a
    // second forever.
    let exporter = exporter(
        PowerSource::recorded(RecordedSource::new(vec![
            Err(SourceError::Exit {
                status: "exit status: 1".to_owned(),
                stderr: "powermetrics must be invoked as the superuser".to_owned(),
            }),
            Err(SourceError::Exit {
                status: "exit status: 1".to_owned(),
                stderr: "powermetrics must be invoked as the superuser".to_owned(),
            }),
            Ok(fixture()),
        ])),
        None,
        Duration::ZERO,
    );

    exposition(&exporter).await;
    exposition(&exporter).await;
    assert_eq!(
        exporter.state_log_count(),
        1,
        "two failed scrapes are one down transition"
    );

    exposition(&exporter).await;
    assert_eq!(
        exporter.state_log_count(),
        2,
        "coming back up is the second transition"
    );
}

#[tokio::test]
async fn scrape_duration_is_published_as_a_number() {
    let exporter = exporter(
        PowerSource::recorded(RecordedSource::text(&fixture())),
        None,
        Duration::from_secs(2),
    );
    let body = exposition(&exporter).await;
    let duration = value_of(&body, "muser_agent_scrape_duration_seconds{agent=\"mac\"}")
        .expect("scrape duration is always published");
    let seconds: f64 = duration.parse().expect("a number");
    assert!((0.0..60.0).contains(&seconds), "{duration}");
}

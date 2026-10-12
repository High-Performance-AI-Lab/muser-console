//! The Prometheus text exposition.
//!
//! Rendering is a pure function of one scrape's outcome, which is what lets
//! every honesty rule be a test rather than a promise:
//!
//! * `devices == None` means the source did not answer. The document is
//!   then `muser_agent_up 0` and literally nothing else — no scrape
//!   duration, no device families, no headers for series that would have no
//!   samples under them.
//! * a `None` field is omitted for that device while the device's other
//!   fields still publish;
//! * a family no device reported is omitted whole, HELP and TYPE included;
//! * a non-finite float is not a measurement and is omitted like a failure.
//!
//! Nothing here rounds, clamps, carries forward, or fills in.

use std::time::Duration;

use crate::source::DeviceSample;

/// The `agent` label every agent-scoped series carries. The console keys
/// its agent catalog off the exporter kind, and this is the gx10 one.
pub const AGENT: &str = "gx10";

/// `Content-Type` for `GET /metrics`, exactly as the exposition format
/// version pins it.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

const UP: &str = "muser_agent_up";
const UP_HELP: &str = "1 when the gx10 agent's NVML source answered this scrape, 0 when it did not; when 0 the exposition carries no other series.";

const SCRAPE_DURATION: &str = "muser_agent_scrape_duration_seconds";
const SCRAPE_DURATION_HELP: &str = "Wall-clock seconds this scrape spent reading NVML.";

const UTILIZATION: &str = "muser_gpu_utilization_ratio";
const UTILIZATION_HELP: &str = "Fraction of the last sampling period the GPU was busy, 0..1. NVML reports whole percent via nvmlDeviceGetUtilizationRates; this exporter divides by 100 and does not clamp.";

const POWER: &str = "muser_gpu_power_watts";
const POWER_HELP: &str = "Board power draw in watts. NVML reports milliwatts via nvmlDeviceGetPowerUsage; this exporter divides by 1000.";

const TEMPERATURE: &str = "muser_gpu_temperature_celsius";
const TEMPERATURE_HELP: &str =
    "GPU die temperature in degrees Celsius, from nvmlDeviceGetTemperature with NVML_TEMPERATURE_GPU.";

const MEMORY_USED: &str = "muser_gpu_memory_used_bytes";
const MEMORY_USED_HELP: &str = "Device memory in use, in bytes, from nvmlDeviceGetMemoryInfo.";

const MEMORY_TOTAL: &str = "muser_gpu_memory_total_bytes";
const MEMORY_TOTAL_HELP: &str = "Total device memory, in bytes, from nvmlDeviceGetMemoryInfo.";

/// Renders one scrape.
///
/// `devices` is `Some` only when the source answered; `Some(&[])` is a
/// source that answered and found no GPUs, which is an honest `up 1` with
/// no device series. `elapsed` is how long the scrape itself took and is
/// published only on an answered scrape — the duration of a failed probe
/// says nothing about the node.
pub fn render(devices: Option<&[DeviceSample]>, elapsed: Duration) -> String {
    let mut out = String::new();

    let Some(devices) = devices else {
        family(
            &mut out,
            UP,
            UP_HELP,
            "gauge",
            &[format!("{UP}{{agent=\"{AGENT}\"}} 0")],
        );
        return out;
    };

    family(
        &mut out,
        UP,
        UP_HELP,
        "gauge",
        &[format!("{UP}{{agent=\"{AGENT}\"}} 1")],
    );
    family(
        &mut out,
        SCRAPE_DURATION,
        SCRAPE_DURATION_HELP,
        "gauge",
        // Printed at full precision rather than to a fixed number of
        // decimals: a fixed width would round a scrape faster than its last
        // place down to a flat zero, and "took no time" is not what a fast
        // scrape measured.
        &[format!(
            "{SCRAPE_DURATION}{{agent=\"{AGENT}\"}} {}",
            elapsed.as_secs_f64()
        )],
    );

    // One pass per family so each family's samples stay contiguous under a
    // single HELP/TYPE pair, as the exposition format requires.
    type Reading = fn(&DeviceSample) -> Option<String>;
    let families: [(&str, &str, Reading); 5] = [
        (UTILIZATION, UTILIZATION_HELP, |device| {
            float(device.utilization_ratio)
        }),
        (POWER, POWER_HELP, |device| float(device.power_watts)),
        (TEMPERATURE, TEMPERATURE_HELP, |device| {
            float(device.temperature_celsius)
        }),
        (MEMORY_USED, MEMORY_USED_HELP, |device| {
            device.memory_used_bytes.map(|bytes| bytes.to_string())
        }),
        (MEMORY_TOTAL, MEMORY_TOTAL_HELP, |device| {
            device.memory_total_bytes.map(|bytes| bytes.to_string())
        }),
    ];

    for (name, help, reading) in families {
        let lines: Vec<String> = devices
            .iter()
            .filter_map(|device| {
                reading(device).map(|value| format!("{name}{{{}}} {value}", labels(device)))
            })
            .collect();
        family(&mut out, name, help, "gauge", &lines);
    }

    out
}

/// Emits `# HELP`, `# TYPE`, then the family's samples.
///
/// A family with no samples is not emitted at all. A HELP/TYPE pair with
/// nothing under it announces a series the exporter does not have, and a
/// scraper that has seen the header will happily draw the gap as one.
fn family(out: &mut String, name: &str, help: &str, kind: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(kind);
    out.push('\n');
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
}

/// The label set identifying one device: always its index, plus whichever
/// of the two driver strings answered. A probe that failed contributes no
/// label rather than an empty or invented one.
fn labels(device: &DeviceSample) -> String {
    let mut labels = format!("gpu=\"{}\"", device.index);
    if let Some(uuid) = &device.uuid {
        labels.push_str(&format!(",uuid=\"{}\"", escape(uuid)));
    }
    if let Some(name) = &device.name {
        labels.push_str(&format!(",name=\"{}\"", escape(name)));
    }
    labels
}

/// Prometheus label-value escaping.
///
/// Device names and UUIDs come from the driver, not from us, so they are
/// escaped rather than trusted. The exposition escape set is exactly
/// backslash, double quote and line feed; nothing else is rewritten,
/// because a value the driver gave should read back as the driver gave it.
pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Formats a float, or drops it. A non-finite reading is not a measurement,
/// and publishing `NaN` would put a number-shaped hole in a chart.
fn float(value: Option<f64>) -> Option<String> {
    value
        .filter(|number| number.is_finite())
        .map(|number| number.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Structural stimulus, not measurements. These values exist to drive
    // the renderer's branches; nothing in this file is or claims to be a
    // captured NVML reading.
    fn full_device(index: u32) -> DeviceSample {
        DeviceSample {
            index,
            uuid: Some(format!("GPU-00000000-0000-0000-0000-00000000000{index}")),
            name: Some("test-device".to_owned()),
            utilization_ratio: Some(0.5),
            power_watts: Some(12.5),
            temperature_celsius: Some(40.0),
            memory_used_bytes: Some(1024),
            memory_total_bytes: Some(4096),
        }
    }

    fn lines_for(text: &str, metric: &str) -> Vec<String> {
        text.lines()
            .filter(|line| line.starts_with(&format!("{metric}{{")))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn a_source_that_did_not_answer_publishes_only_agent_up_zero() {
        let text = render(None, Duration::from_millis(7));
        assert_eq!(
            text,
            "# HELP muser_agent_up 1 when the gx10 agent's NVML source answered this scrape, \
             0 when it did not; when 0 the exposition carries no other series.\n\
             # TYPE muser_agent_up gauge\n\
             muser_agent_up{agent=\"gx10\"} 0\n"
        );
        assert!(
            !text.contains("muser_gpu_"),
            "no device series without a source"
        );
        assert!(
            !text.contains(SCRAPE_DURATION),
            "a failed probe's duration describes nothing about the node"
        );
    }

    #[test]
    fn an_answered_scrape_publishes_up_one_and_its_duration() {
        let text = render(Some(&[]), Duration::from_millis(1500));
        assert!(text.contains("muser_agent_up{agent=\"gx10\"} 1\n"));
        assert!(text.contains("muser_agent_scrape_duration_seconds{agent=\"gx10\"} 1.5\n"));
        assert!(
            !text.contains("muser_gpu_"),
            "a working NVML with no GPUs still publishes no device series"
        );
    }

    #[test]
    fn every_published_family_carries_help_and_type() {
        let text = render(Some(&[full_device(0)]), Duration::from_millis(1));
        for metric in [
            UP,
            SCRAPE_DURATION,
            UTILIZATION,
            POWER,
            TEMPERATURE,
            MEMORY_USED,
            MEMORY_TOTAL,
        ] {
            assert!(
                text.contains(&format!("# HELP {metric} ")),
                "{metric} needs HELP"
            );
            assert!(
                text.contains(&format!("# TYPE {metric} gauge\n")),
                "{metric} needs TYPE"
            );
        }
    }

    #[test]
    fn a_failed_field_probe_omits_only_that_field() {
        let mut device = full_device(0);
        device.power_watts = None;
        device.temperature_celsius = None;
        let text = render(Some(&[device]), Duration::from_millis(1));

        assert!(
            !text.contains(POWER) && !text.contains(TEMPERATURE),
            "a field NVML did not answer is absent, header and all:\n{text}"
        );
        assert_eq!(lines_for(&text, UTILIZATION).len(), 1);
        assert_eq!(lines_for(&text, MEMORY_USED).len(), 1);
        assert_eq!(lines_for(&text, MEMORY_TOTAL).len(), 1);
    }

    #[test]
    fn one_devices_failed_probe_does_not_silence_its_siblings() {
        let mut second = full_device(1);
        second.power_watts = None;
        let text = render(Some(&[full_device(0), second]), Duration::from_millis(1));

        let power = lines_for(&text, POWER);
        assert_eq!(power.len(), 1, "only gpu 0 reported power");
        assert!(power[0].contains("gpu=\"0\""));
        assert_eq!(lines_for(&text, UTILIZATION).len(), 2);
    }

    #[test]
    fn a_device_whose_driver_strings_failed_still_publishes_its_readings() {
        let mut device = full_device(0);
        device.uuid = None;
        device.name = None;
        let text = render(Some(&[device]), Duration::from_millis(1));

        assert!(text.contains("muser_gpu_utilization_ratio{gpu=\"0\"} 0.5\n"));
        assert!(
            !text.contains("uuid=") && !text.contains("name="),
            "a label whose probe failed is absent, not empty"
        );
    }

    #[test]
    fn label_values_are_escaped() {
        let device = DeviceSample {
            index: 0,
            uuid: Some("GPU-back\\slash".to_owned()),
            name: Some("NVIDIA \"GB10\"\nrev\\2".to_owned()),
            utilization_ratio: Some(0.25),
            ..DeviceSample::default()
        };
        let text = render(Some(&[device]), Duration::from_millis(1));
        let line = lines_for(&text, UTILIZATION).remove(0);

        assert_eq!(
            line,
            "muser_gpu_utilization_ratio{gpu=\"0\",uuid=\"GPU-back\\\\slash\",\
             name=\"NVIDIA \\\"GB10\\\"\\nrev\\\\2\"} 0.25"
        );
        // up (3) + scrape duration (3) + the one utilization family (3).
        assert_eq!(
            text.lines().count(),
            9,
            "an embedded newline must not split the exposition into extra lines:\n{text}"
        );
    }

    #[test]
    fn escape_touches_only_the_three_exposition_escapes() {
        assert_eq!(escape("plain"), "plain");
        assert_eq!(escape("a\\b"), "a\\\\b");
        assert_eq!(escape("a\"b"), "a\\\"b");
        assert_eq!(escape("a\nb"), "a\\nb");
        assert_eq!(
            escape("tab\there"),
            "tab\there",
            "a tab is a legal label byte"
        );
        assert_eq!(escape("näme µ"), "näme µ", "UTF-8 passes through");
    }

    #[test]
    fn non_finite_readings_are_omitted_like_failures() {
        let device = DeviceSample {
            index: 0,
            utilization_ratio: Some(f64::NAN),
            power_watts: Some(f64::INFINITY),
            temperature_celsius: Some(f64::NEG_INFINITY),
            ..DeviceSample::default()
        };
        let text = render(Some(&[device]), Duration::from_millis(1));
        assert!(!text.contains("muser_gpu_"), "got:\n{text}");
        assert!(!text.contains("NaN") && !text.contains("inf"));
    }

    #[test]
    fn samples_of_a_family_stay_contiguous_under_one_header() {
        let text = render(
            Some(&[full_device(0), full_device(1), full_device(2)]),
            Duration::from_millis(1),
        );
        let lines: Vec<&str> = text.lines().collect();
        let start = lines
            .iter()
            .position(|line| *line == format!("# TYPE {UTILIZATION} gauge"))
            .expect("utilization family present");
        for offset in 1..=3 {
            assert!(
                lines[start + offset].starts_with(&format!("{UTILIZATION}{{gpu=\"")),
                "sample {offset} follows its own family header"
            );
        }
        assert_eq!(
            text.matches(&format!("# HELP {UTILIZATION} ")).count(),
            1,
            "one HELP per family, not one per device"
        );
    }

    #[test]
    fn help_text_never_breaks_the_comment_line() {
        for help in [
            UP_HELP,
            SCRAPE_DURATION_HELP,
            UTILIZATION_HELP,
            POWER_HELP,
            TEMPERATURE_HELP,
            MEMORY_USED_HELP,
            MEMORY_TOTAL_HELP,
        ] {
            assert!(!help.is_empty());
            assert!(!help.contains('\n'), "HELP is one line: {help}");
            assert!(!help.contains('\\'), "HELP needs no escaping: {help}");
        }
    }
}

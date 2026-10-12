//! What the exporter's data source is allowed to say, and the single shape
//! it says it in.
//!
//! The renderer never sees NVML; it sees `DeviceSample`s. That boundary is
//! what makes the omit-on-failure rule checkable without a GPU: a probe
//! that failed is a `None` field here, and `expo` has no way to turn a
//! `None` into a published number.

/// One GPU's readings for one scrape.
///
/// Every reading is optional and `None` means exactly one thing: that NVML
/// call did not answer for this device on this scrape, so the exporter
/// publishes nothing for it. `index` is the only field that is not a probe
/// — it is the enumeration position the handle was fetched at, which is why
/// it is always known.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceSample {
    pub index: u32,
    pub uuid: Option<String>,
    pub name: Option<String>,
    /// 0..1. NVML reports whole percent; the conversion lives in `nvml`.
    pub utilization_ratio: Option<f64>,
    /// Watts. NVML reports milliwatts.
    pub power_watts: Option<f64>,
    pub temperature_celsius: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
}

impl DeviceSample {
    /// A device with a known index and no readings yet — the state every
    /// sample starts in, so a field is published only by being filled in.
    pub fn new(index: u32) -> DeviceSample {
        DeviceSample {
            index,
            ..DeviceSample::default()
        }
    }
}

/// `Ok` is what the source reported this scrape (possibly an empty device
/// list — a host with a working NVML and no GPUs). `Err` carries a short,
/// human-readable reason the source could not answer at all, which reaches
/// the log and never the exposition body.
pub type ScrapeResult = Result<Vec<DeviceSample>, String>;

/// Where readings come from. One implementation talks to NVML; the other
/// replays what a test hands it.
pub trait GpuSource: Send + Sync + 'static {
    fn scrape(&self) -> ScrapeResult;
}

/// A source that replays a fixed outcome.
///
/// This makes no measurement claim of its own: it returns whatever it was
/// constructed with, which is why it is only ever constructed by tests. The
/// binary has no flag that selects it, so no deployed exporter can publish
/// anything but NVML's answers.
pub struct RecordedSource {
    outcome: ScrapeResult,
}

impl RecordedSource {
    /// A source that answers with `devices`.
    pub fn devices(devices: Vec<DeviceSample>) -> RecordedSource {
        RecordedSource {
            outcome: Ok(devices),
        }
    }

    /// A source that did not answer — the NVML-absent path.
    pub fn failing(reason: &str) -> RecordedSource {
        RecordedSource {
            outcome: Err(reason.to_owned()),
        }
    }
}

impl GpuSource for RecordedSource {
    fn scrape(&self) -> ScrapeResult {
        self.outcome.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_device_sample_holds_no_readings() {
        let sample = DeviceSample::new(3);
        assert_eq!(sample.index, 3);
        assert_eq!(
            sample,
            DeviceSample {
                index: 3,
                ..DeviceSample::default()
            }
        );
        assert!(sample.utilization_ratio.is_none());
        assert!(sample.power_watts.is_none());
        assert!(sample.temperature_celsius.is_none());
        assert!(sample.memory_used_bytes.is_none());
        assert!(sample.memory_total_bytes.is_none());
        assert!(sample.uuid.is_none());
        assert!(sample.name.is_none());
    }

    #[test]
    fn a_failing_source_reports_a_reason_and_no_devices() {
        let source = RecordedSource::failing("NVML not loadable");
        assert_eq!(source.scrape(), Err("NVML not loadable".to_owned()));
    }
}

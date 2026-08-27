//! Parser for `powermetrics` text output.
//!
//! ## Why the text format and not `--format plist`
//!
//! The plist format is machine-readable, which argues for it — but it is a
//! NUL-separated XML property list, and reading it means either adding an XML
//! dependency (outside this project's dependency budget) or hand-rolling an
//! XML parser whose key names I would have to *guess*, because `powermetrics`
//! cannot be run on the build host to find out (it refuses without root and
//! this project does not escalate privileges to satisfy a parser). Writing a
//! parser around invented key names would be exactly the kind of plausible
//! fiction this repository exists to avoid.
//!
//! The text format's power lines are one grammar rule wide:
//!
//! ```text
//! <label>: <number><unit>
//! ```
//!
//! so the parser below is a *recognizer*, not a schema: it reads the lines it
//! recognizes and ignores every other byte in the stream. It never assumes a
//! line is present. If this machine's `powermetrics` prints a label under a
//! different name, the corresponding series is simply absent — which is the
//! honest report of a field this exporter could not read.
//!
//! ## Which line becomes which series
//!
//! `powermetrics` does not print the same package-power label on every Mac,
//! and this exporter cannot know in advance which one the host uses. Rather
//! than assert one, it recognizes the known spellings and reports *which line
//! the number came from* in [`PowerField::source_label`] — the exposition
//! prints that label in a comment, so a reader never has to guess what the
//! package-power number is a sum of.

/// One power value read from one `powermetrics` line.
#[derive(Clone, Debug, PartialEq)]
pub struct PowerField {
    /// Watts. `powermetrics` prints milliwatts on some platforms and watts on
    /// others; the unit is on the line itself and is converted from what was
    /// printed, never assumed.
    pub watts: f64,
    /// The exact label of the line this number was read from, so the
    /// exposition can say where it came from.
    pub source_label: String,
}

/// The power fields one `powermetrics` sample yielded. Every field is
/// optional: a field the output did not contain is `None`, and `None` is
/// published as nothing at all.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    pub package: Option<PowerField>,
    pub cpu: Option<PowerField>,
    pub gpu: Option<PowerField>,
}

impl Reading {
    /// True when the output contained none of the recognized power lines.
    /// The exporter still reports the source as up — it answered — but it
    /// publishes no power series, because there were none to publish.
    pub fn is_empty(&self) -> bool {
        self.package.is_none() && self.cpu.is_none() && self.gpu.is_none()
    }
}

/// Labels that carry combined package power, in preference order. The first
/// one present in the output wins; the exposition names the winner.
///
/// These are the spellings `powermetrics` is known to use across Apple
/// Silicon and Intel Macs. Listing several is not a claim that any given
/// machine prints a particular one — it is what lets the exporter read
/// whichever the host actually printed, and report nothing when it printed
/// none of them.
const PACKAGE_LABELS: &[&str] = &[
    "Package Power",
    "Combined Power (CPU + GPU + ANE)",
    "Intel energy model derived package power (CPUs+GT+SA)",
];

const CPU_LABELS: &[&str] = &["CPU Power"];
const GPU_LABELS: &[&str] = &["GPU Power"];

/// Read every recognized power line out of `text`.
///
/// Unrecognized lines — headers, residencies, frequencies, blank lines, the
/// `#` comments in this crate's structure fixture — contribute nothing. Where
/// a label appears more than once (a `-n 1` run still prints `GPU Power` in
/// both the processor and GPU sections) the first occurrence wins, and later
/// ones are ignored rather than averaged: averaging two numbers the tool
/// printed separately would publish a value the tool never reported.
pub fn parse_powermetrics(text: &str) -> Reading {
    let mut reading = Reading::default();
    for line in text.lines() {
        let Some((label, watts)) = parse_power_line(line) else {
            continue;
        };
        if reading.package.is_none() && PACKAGE_LABELS.contains(&label) {
            reading.package = Some(field(label, watts));
        } else if reading.cpu.is_none() && CPU_LABELS.contains(&label) {
            reading.cpu = Some(field(label, watts));
        } else if reading.gpu.is_none() && GPU_LABELS.contains(&label) {
            reading.gpu = Some(field(label, watts));
        }
    }
    reading
}

fn field(label: &str, watts: f64) -> PowerField {
    PowerField {
        watts,
        source_label: label.to_owned(),
    }
}

/// `<label>: <number><unit>` where unit is `mW` or `W`, with or without a
/// space before it (Apple Silicon prints `1234 mW`, the Intel package line
/// prints `5.23W`). Anything else — another unit, trailing words, a
/// non-finite number — is not a power line and yields `None`.
fn parse_power_line(line: &str) -> Option<(&str, f64)> {
    let (label, rest) = line.split_once(':')?;
    let label = label.trim();
    if label.is_empty() {
        return None;
    }
    let rest = rest.trim();
    let split = rest
        .find(|character: char| !matches!(character, '0'..='9' | '.' | '-' | '+'))
        .unwrap_or(rest.len());
    let (number, unit) = rest.split_at(split);
    let value: f64 = number.parse().ok()?;
    // A non-finite reading is not a measurement anyone can chart, and the
    // console refuses to store one; drop it here rather than publish it.
    if !value.is_finite() {
        return None;
    }
    let watts = match unit.trim() {
        "mW" => value / 1000.0,
        "W" => value,
        _ => return None,
    };
    Some((label, watts))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every string in this module is a *parser input* written to exercise a
    // grammar rule. None of these numbers is a measurement of any machine,
    // and no assertion below treats one as a fact about hardware.

    #[test]
    fn reads_milliwatt_and_watt_lines_and_converts_by_the_printed_unit() {
        let reading = parse_powermetrics("CPU Power: 1234 mW\nPackage Power: 2.5W\n");
        assert_eq!(reading.cpu.as_ref().expect("cpu line").watts, 1.234);
        assert_eq!(reading.package.as_ref().expect("package line").watts, 2.5);
        assert_eq!(reading.gpu, None, "no GPU line in the input, no GPU field");
    }

    #[test]
    fn package_labels_are_tried_in_preference_order_and_the_winner_is_recorded() {
        let combined = parse_powermetrics("Combined Power (CPU + GPU + ANE): 500 mW\n");
        assert_eq!(
            combined
                .package
                .as_ref()
                .expect("combined line")
                .source_label,
            "Combined Power (CPU + GPU + ANE)"
        );

        let intel =
            parse_powermetrics("Intel energy model derived package power (CPUs+GT+SA): 5.23W\n");
        assert_eq!(intel.package.as_ref().expect("intel line").watts, 5.23);

        let both = parse_powermetrics(
            "Package Power: 1000 mW\nCombined Power (CPU + GPU + ANE): 900 mW\n",
        );
        let package = both.package.as_ref().expect("package line");
        assert_eq!(package.source_label, "Package Power");
        assert_eq!(package.watts, 1.0);
    }

    #[test]
    fn first_occurrence_of_a_label_wins() {
        let reading = parse_powermetrics("GPU Power: 10 mW\nGPU Power: 40 mW\n");
        assert_eq!(reading.gpu.as_ref().expect("gpu line").watts, 0.01);
    }

    #[test]
    fn other_units_labels_and_shapes_are_not_power_lines() {
        let reading = parse_powermetrics(
            "\
*** Sampled system activity ***
GPU HW active frequency: 400 MHz
GPU idle residency:  97.00%
ANE Power: 0 mW
DRAM Power: 111 mW
CPU Power: 1234 mW extra
CPU Power: not-a-number mW
: 5 mW
CPU Power
",
        );
        assert_eq!(
            reading,
            Reading::default(),
            "nothing here is a recognized power line"
        );
    }

    #[test]
    fn values_that_are_not_finite_numbers_are_dropped() {
        // "NaN"/"inf" never reach the float parser: the value token stops at
        // the first non-numeric byte, so these are not power lines at all.
        let words = parse_powermetrics("CPU Power: NaN mW\nGPU Power: inf W\n");
        assert!(words.is_empty(), "non-numeric readings publish nothing");
        // A digit string too large for f64 does parse — as infinity, which is
        // not a measurement; it is dropped rather than published.
        let overflow = format!("CPU Power: {} mW\n", "9".repeat(400));
        assert!(
            parse_powermetrics(&overflow).is_empty(),
            "an overflowing reading publishes nothing"
        );
    }

    #[test]
    fn empty_output_is_empty_not_zero() {
        let reading = parse_powermetrics("");
        assert!(reading.is_empty());
        assert_eq!(reading.package, None);
    }
}

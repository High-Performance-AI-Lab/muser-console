//! A small hand-rolled parser for the engine's Prometheus text exposition.
//!
//! The engine hand-formats its exposition (docs/engine-contract.md): lines
//! are `name value` or `name{label="v"} value`, with no honesty, node, or
//! instance labels. This parser accepts that shape plus the standard
//! comment lines and optional trailing timestamp, and silently drops any
//! line it cannot read — a line the console cannot parse contributes no
//! sample, which is the honest outcome.
//!
//! No dependency is pulled in for this: the grammar is four rules wide and
//! the console's dependency budget is the point.

/// One parsed exposition line.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub metric: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Sample {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

/// A parsed exposition document.
#[derive(Clone, Debug, Default)]
pub struct Exposition {
    samples: Vec<Sample>,
}

impl Exposition {
    pub fn parse(text: &str) -> Exposition {
        Exposition {
            samples: text.lines().filter_map(parse_line).collect(),
        }
    }

    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// First sample named `metric` whose `key` label is exactly `wanted`.
    /// Used for the agent exporters' per-device series (`gpu="0"`); a device
    /// the exporter did not publish yields `None`, never a neighbour's value.
    pub fn labeled_value(&self, metric: &str, key: &str, wanted: &str) -> Option<f64> {
        self.samples
            .iter()
            .find(|sample| sample.metric == metric && sample.label(key) == Some(wanted))
            .map(|sample| sample.value)
    }

    /// First sample named `metric`, optionally restricted to a `quantile`
    /// label. Returns the raw value including non-finite ones; the caller
    /// decides what is storable.
    pub fn value(&self, metric: &str, quantile: Option<&str>) -> Option<f64> {
        self.samples
            .iter()
            .find(|sample| {
                sample.metric == metric
                    && match quantile {
                        Some(wanted) => sample.label("quantile") == Some(wanted),
                        None => true,
                    }
            })
            .map(|sample| sample.value)
    }
}

fn parse_line(line: &str) -> Option<Sample> {
    let line = line.trim();
    // Comments (`# HELP` / `# TYPE`) and blank lines carry no samples.
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let (metric, labels, rest) = match line.find('{') {
        Some(open) => {
            let close = close_brace(line, open)?;
            let labels = parse_labels(&line[open + 1..close])?;
            (&line[..open], labels, &line[close + 1..])
        }
        None => {
            let split = line.find(char::is_whitespace)?;
            (&line[..split], Vec::new(), &line[split..])
        }
    };

    let metric = metric.trim();
    if !valid_metric_name(metric) {
        return None;
    }
    // Prometheus permits a trailing millisecond timestamp; the engine emits
    // none, and either way only the value is a measurement.
    let value: f64 = rest.split_whitespace().next()?.parse().ok()?;
    Some(Sample {
        metric: metric.to_owned(),
        labels,
        value,
    })
}

/// Index of the `}` closing the label set opened at `open`, skipping any
/// brace that sits inside a quoted label value.
fn close_brace(line: &str, open: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut index = open + 1;
    let mut in_quotes = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if in_quotes => index += 1,
            b'"' => in_quotes = !in_quotes,
            b'}' if !in_quotes => return Some(index),
            _ => {}
        }
        index += 1;
    }
    None
}

fn parse_labels(text: &str) -> Option<Vec<(String, String)>> {
    let mut labels = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            return Some(labels);
        }
        let key_start = index;
        while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
        {
            index += 1;
        }
        let key = text.get(key_start..index)?;
        if key.is_empty() {
            return None;
        }
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) != Some(&b'=') {
            return None;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) != Some(&b'"') {
            return None;
        }
        index += 1;
        let mut value = String::new();
        loop {
            match bytes.get(index) {
                None => return None,
                Some(b'"') => {
                    index += 1;
                    break;
                }
                Some(b'\\') => {
                    // The exposition escape set is exactly \\, \" and \n.
                    let escaped = bytes.get(index + 1)?;
                    value.push(match escaped {
                        b'n' => '\n',
                        b'\\' => '\\',
                        b'"' => '"',
                        _ => return None,
                    });
                    index += 2;
                }
                Some(_) => {
                    let start = index;
                    while index < bytes.len() && bytes[index] != b'"' && bytes[index] != b'\\' {
                        index += 1;
                    }
                    value.push_str(text.get(start..index)?);
                }
            }
        }
        labels.push((key.to_owned(), value));
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        match bytes.get(index) {
            Some(b',') => index += 1,
            None => return Some(labels),
            Some(_) => return None,
        }
    }
}

fn valid_metric_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b':') {
        return false;
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b':')
}

// Structural grammar tests live here, next to the grammar. The parser is
// also exercised against an engine-format exposition built from real
// captured fixture values in tests/history.rs, where the fixture-reading
// helpers live.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_blank_lines_and_junk_are_dropped() {
        let text = "\
# HELP muser_queue_depth requests accepted but not past first token
# TYPE muser_queue_depth gauge
muser_queue_depth 3

muser_missing_value
muser_bad_value abc
muser_unclosed{quantile=\"0.50\" 1
{quantile=\"0.50\"} 1
9_bad_name 1
muser_queue_depth_2 4
";
        let exposition = Exposition::parse(text);
        assert_eq!(
            exposition.samples().len(),
            2,
            "only the two well-formed lines"
        );
        assert_eq!(exposition.value("muser_queue_depth", None), Some(3.0));
        assert_eq!(exposition.value("muser_queue_depth_2", None), Some(4.0));
        assert_eq!(exposition.value("muser_missing_value", None), None);
        assert_eq!(exposition.value("muser_bad_value", None), None);
    }

    #[test]
    fn label_edge_cases() {
        let text = "\
a{x=\"1\",y=\"two\"} 1
b{ x = \"sp aced\" } 2
c{x=\"br}ace\"} 3
d{x=\"quo\\\"te\",y=\"back\\\\slash\"} 4
e{} 5
f{x=\"unterminated} 6
g{x=1} 7
";
        let exposition = Exposition::parse(text);
        assert_eq!(exposition.value("a", None), Some(1.0));
        assert_eq!(exposition.samples()[0].label("y"), Some("two"));
        assert_eq!(exposition.value("b", None), Some(2.0));
        assert_eq!(exposition.samples()[1].label("x"), Some("sp aced"));
        assert_eq!(exposition.value("c", None), Some(3.0));
        assert_eq!(exposition.samples()[2].label("x"), Some("br}ace"));
        assert_eq!(exposition.value("d", None), Some(4.0));
        assert_eq!(exposition.samples()[3].label("x"), Some("quo\"te"));
        assert_eq!(exposition.samples()[3].label("y"), Some("back\\slash"));
        assert_eq!(exposition.value("e", None), Some(5.0));
        assert_eq!(exposition.value("f", None), None, "unterminated quote");
        assert_eq!(exposition.value("g", None), None, "unquoted label value");
    }

    #[test]
    fn trailing_timestamp_is_ignored_and_non_finite_values_survive_parsing() {
        let exposition = Exposition::parse("a 1.5 1755300000000\nb NaN\nc +Inf\n");
        assert_eq!(exposition.value("a", None), Some(1.5));
        assert!(exposition.value("b", None).expect("parsed").is_nan());
        assert_eq!(exposition.value("c", None), Some(f64::INFINITY));
    }

    #[test]
    fn labeled_lookup_selects_one_device_and_never_a_neighbour() {
        // Structural: two devices, ordinal values, no measurement claimed.
        let text = "\
muser_gpu_power_watts{gpu=\"0\",uuid=\"GPU-a\",name=\"Dev A\"} 1
muser_gpu_power_watts{gpu=\"1\",uuid=\"GPU-b\",name=\"Dev, B\"} 2
";
        let exposition = Exposition::parse(text);
        assert_eq!(
            exposition.labeled_value("muser_gpu_power_watts", "gpu", "0"),
            Some(1.0)
        );
        assert_eq!(
            exposition.labeled_value("muser_gpu_power_watts", "gpu", "1"),
            Some(2.0)
        );
        assert_eq!(
            exposition.labeled_value("muser_gpu_power_watts", "gpu", "2"),
            None,
            "a device the exporter did not publish has no value at all"
        );
        assert_eq!(
            exposition.samples()[1].label("name"),
            Some("Dev, B"),
            "a driver-supplied name with a comma survives the label parser"
        );
    }

    #[test]
    fn empty_document_is_empty_not_zero() {
        let exposition = Exposition::parse("");
        assert!(exposition.is_empty());
        assert_eq!(exposition.value("muser_queue_depth", None), None);
    }
}

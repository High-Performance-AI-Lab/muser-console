//! Rendering the Prometheus exposition.
//!
//! Every metric that is emitted carries `# HELP` and `# TYPE`. A metric with
//! no value is not emitted at all — not as an empty family, not as a zero. The
//! reader of this text can therefore treat "absent" as "this exporter did not
//! measure it", which is the whole point.

use std::time::Duration;

use crate::exporter::{Scrape, Served};
use crate::parse::PowerField;

/// Prometheus text exposition, the version the engine also speaks.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The `agent` label the agent-level series carry. The console keys its agent
/// catalog off the metric names, not this, but a human reading two exporters'
/// output side by side needs to tell them apart.
pub const AGENT: &str = "mac";

const PACKAGE: (&str, &str) = (
    "muser_host_package_power_watts",
    "Combined package power for this host, as reported by powermetrics.",
);
const CPU: (&str, &str) = (
    "muser_host_cpu_power_watts",
    "CPU power for this host, as reported by the powermetrics cpu_power sampler.",
);
const GPU: (&str, &str) = (
    "muser_host_gpu_power_watts",
    "GPU power for this host, as reported by the powermetrics gpu_power sampler.",
);

/// Render one scrape. `max_age` is the window this exporter will reuse a
/// reading for, and is stated in the exposition so a reader knows how old the
/// number in front of them is allowed to be.
pub fn render(scrape: &Scrape, host: Option<&str>, max_age: Duration) -> String {
    let mut out = String::with_capacity(1024);
    let agent = labels(&[("agent", AGENT)]);

    out.push_str(
        "# HELP muser_agent_up 1 when this scrape served a real powermetrics reading, 0 when \
         the exporter could not obtain one.\n",
    );
    out.push_str("# TYPE muser_agent_up gauge\n");
    out.push_str(&format!(
        "muser_agent_up{agent} {}\n",
        if scrape.up { 1 } else { 0 }
    ));

    match &scrape.served {
        // The sample's own timestamp goes in a comment. Publishing it as the
        // series timestamp would let a reused reading pass for a fresh one.
        Some(served) => {
            // Only a scrape that served a reading publishes a duration: the
            // time a failure took describes nothing about the machine. Same
            // shape as the gx10 exporter's down document.
            out.push_str(
                "# HELP muser_agent_scrape_duration_seconds Wall-clock seconds this scrape \
                 spent obtaining its reading; near zero when a reading taken moments ago was \
                 reused.\n",
            );
            out.push_str("# TYPE muser_agent_scrape_duration_seconds gauge\n");
            // Unrounded: a cache hit takes microseconds, and {:.6} would
            // print that as a flat 0.000000.
            out.push_str(&format!(
                "muser_agent_scrape_duration_seconds{agent} {}\n",
                scrape.duration.as_secs_f64()
            ));
            // The served reading's age, machine-readable rather than only in
            // the comment below: a scraper can tell a fresh measurement from
            // a reused one without parsing prose.
            out.push_str(
                "# HELP muser_agent_sample_age_seconds Age of the powermetrics sample this \
                 scrape served, at the moment it was served.\n",
            );
            out.push_str("# TYPE muser_agent_sample_age_seconds gauge\n");
            out.push_str(&format!(
                "muser_agent_sample_age_seconds{agent} {}\n",
                served.age.as_secs_f64()
            ));
            out.push_str(&provenance_comment(served, max_age));
            render_field(&mut out, host, PACKAGE, served.reading.package.as_ref());
            render_field(&mut out, host, CPU, served.reading.cpu.as_ref());
            render_field(&mut out, host, GPU, served.reading.gpu.as_ref());
        }
        None => {
            if let Some(reason) = scrape.failure {
                out.push_str(&format!(
                    "# no reading this scrape: {reason}; the exporter's stderr log has the \
                     reason. No power series are published and no earlier reading is reused.\n"
                ));
            }
        }
    }
    out
}

fn provenance_comment(served: &Served, max_age: Duration) -> String {
    let age = served.age.as_secs_f64();
    let window = max_age.as_secs_f64();
    let tail = format!(
        " A reading is served for at most {window:.3} s and then re-measured; an older one is \
         never served."
    );
    match served.completed_unix_s {
        Some(unix) => format!(
            "# powermetrics sample completed at unix {unix:.3}, {age:.3} s before this \
             scrape.{tail}\n"
        ),
        None => {
            format!("# powermetrics sample completed {age:.3} s before this scrape.{tail}\n")
        }
    }
}

fn render_field(
    out: &mut String,
    host: Option<&str>,
    (metric, help): (&str, &str),
    field: Option<&PowerField>,
) {
    // A field powermetrics did not print publishes nothing at all; the other
    // fields of the same sample still publish.
    let Some(field) = field else {
        return;
    };
    // Naming the exact source line keeps the number auditable: a reader never
    // has to guess which of powermetrics' spellings this came from, or what it
    // is a sum of.
    out.push_str(&format!(
        "# {metric} is read from the powermetrics line \"{}\".\n",
        escape_label_value(&field.source_label)
    ));
    out.push_str(&format!("# HELP {metric} {help}\n"));
    out.push_str(&format!("# TYPE {metric} gauge\n"));
    // The source line is a label as well as a comment: comments are stripped
    // by every Prometheus parser, so a comment-only provenance reaches no
    // consumer. Which sum this is differs by hardware, and a reader must be
    // able to tell without guessing.
    let labels = match host {
        Some(host) => labels(&[("host", host), ("source", &field.source_label)]),
        // No host label rather than an invented one; see host.rs.
        None => labels(&[("source", &field.source_label)]),
    };
    out.push_str(&format!("{metric}{labels} {}\n", value(field.watts)));
}

/// `{a="1",b="2"}`, or the empty string for no labels.
fn labels(pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let mut out = String::from("{");
    for (index, (name, value)) in pairs.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(name);
        out.push_str("=\"");
        out.push_str(&escape_label_value(value));
        out.push('"');
    }
    out.push('}');
    out
}

/// The exposition escape set is exactly backslash, quote and newline. Label
/// values here come from the machine's own hostname and from powermetrics'
/// output, neither of which this exporter controls.
pub fn escape_label_value(value: &str) -> String {
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

/// Prometheus wants a plain decimal; Rust's shortest-roundtrip float
/// formatting gives one, and gives back exactly the value that was parsed.
fn value(watts: f64) -> String {
    format!("{watts}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_exactly_the_three_defined_sequences() {
        assert_eq!(
            escape_label_value("a\\b\"c\nd"),
            "a\\\\b\\\"c\\nd",
            "backslash, quote and newline"
        );
        assert_eq!(escape_label_value("plain-name.local"), "plain-name.local");
        assert_eq!(escape_label_value("é"), "é", "UTF-8 passes through");
    }

    #[test]
    fn label_sets_render_or_vanish() {
        assert_eq!(labels(&[]), "");
        assert_eq!(labels(&[("agent", "mac")]), "{agent=\"mac\"}");
        assert_eq!(
            labels(&[("host", "we\"ird"), ("agent", "mac")]),
            "{host=\"we\\\"ird\",agent=\"mac\"}"
        );
    }

    #[test]
    fn values_round_trip_through_formatting() {
        assert_eq!(value(1.234), "1.234");
        assert_eq!(value(0.0), "0");
        assert_eq!(value(-0.5), "-0.5");
        assert_eq!(value(0.05625).parse::<f64>().expect("parses back"), 0.05625);
    }
}

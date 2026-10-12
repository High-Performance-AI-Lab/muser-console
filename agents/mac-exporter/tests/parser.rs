//! The powermetrics text parser, driven by the checked-in fixture.
//!
//! Read the header of `tests/fixtures/powermetrics-cpu-gpu-power.structure.txt`
//! before adding an assertion here. That file is a parser-shape fixture, not a
//! capture: every assertion below is of the form "these bytes map to this
//! field", and none of them treats a number in the fixture as a fact about any
//! machine's power draw.

use std::path::PathBuf;

use mac_exporter::parse::{parse_powermetrics, Reading};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/powermetrics-cpu-gpu-power.structure.txt")
}

fn fixture() -> String {
    std::fs::read_to_string(fixture_path()).expect("structure fixture must be readable")
}

#[test]
fn fixture_says_what_it_is() {
    let text = fixture();
    assert!(
        text.contains("STRUCTURE FIXTURE — NOT A MEASUREMENT"),
        "the fixture must state that it is not a measurement"
    );
    assert!(
        text.contains("NONE of the numbers below is a measurement"),
        "the fixture must disclaim its numbers"
    );
}

#[test]
fn each_recognized_line_maps_to_its_field_with_its_unit_converted() {
    let reading = parse_powermetrics(&fixture());

    let package = reading.package.as_ref().expect("Package Power line");
    assert_eq!(package.source_label, "Package Power");
    // 1290.5 mW as written in the fixture, divided by 1000. IEEE division is
    // correctly rounded, so this is bit-identical to the literal.
    assert_eq!(package.watts, 1.2905);

    let cpu = reading.cpu.as_ref().expect("CPU Power line");
    assert_eq!(cpu.source_label, "CPU Power");
    assert_eq!(cpu.watts, 1.234);

    let gpu = reading.gpu.as_ref().expect("GPU Power line");
    assert_eq!(gpu.source_label, "GPU Power");
    assert_eq!(gpu.watts, 0.05625);
}

#[test]
fn the_fixtures_header_comments_change_nothing() {
    let text = fixture();
    let without_header: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert_ne!(text, without_header, "the fixture does carry a header");
    assert_eq!(
        parse_powermetrics(&text),
        parse_powermetrics(&without_header),
        "unrecognized lines contribute nothing, header or not"
    );
}

#[test]
fn frequencies_residencies_and_unlisted_power_lines_are_not_read() {
    let reading = parse_powermetrics(&fixture());
    // ANE and DRAM power are in the fixture and are deliberately not mapped:
    // this exporter publishes package, CPU and GPU power and nothing else, so
    // there is no field for them to land in.
    assert!(fixture().contains("ANE Power: 0 mW"));
    assert!(fixture().contains("GPU HW active frequency: 400 MHz"));
    assert_eq!(
        reading
            .package
            .as_ref()
            .map(|field| field.source_label.clone()),
        Some("Package Power".to_owned()),
        "the MHz and % lines did not become a power field"
    );
}

#[test]
fn a_sample_missing_a_line_yields_no_field_for_it() {
    // Only one recognized line: the other two fields stay absent rather than
    // becoming zero.
    let reading = parse_powermetrics("**** Processor usage ****\n\nCPU Power: 1234 mW\n");
    assert_eq!(reading.cpu.as_ref().expect("cpu line").watts, 1.234);
    assert_eq!(reading.package, None);
    assert_eq!(reading.gpu, None);
    assert!(!reading.is_empty());
}

#[test]
fn output_with_nothing_recognizable_yields_nothing() {
    let reading = parse_powermetrics("powermetrics must be invoked as the superuser\n");
    assert_eq!(reading, Reading::default());
    assert!(reading.is_empty());
}

//! Conformance: the verbatim published schema and console-owned runtime
//! extension contract compile as Draft 2020-12. Current live snapshots must
//! satisfy the runtime contract and satisfy the published contract after
//! removing only the explicitly documented runtime-only fields.

mod common;

use std::path::PathBuf;

fn compile_schema(name: &str) -> jsonschema::Validator {
    let path = common::repo_root().join("schema").join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let schema: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} parses as JSON: {error}", path.display()));
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&schema)
        .unwrap_or_else(|error| panic!("{} compiles as Draft 2020-12: {error}", path.display()))
}

fn published_projection(snapshot: &serde_json::Value) -> serde_json::Value {
    let mut projected = snapshot.clone();
    let object = projected
        .as_object_mut()
        .expect("captured snapshot top level is an object");
    object.remove("_dflash_acceptance");
    if let Some(transfers) = object
        .get_mut("transfers")
        .and_then(|value| value.as_array_mut())
    {
        for transfer in transfers {
            if let Some(transfer) = transfer.as_object_mut() {
                transfer.remove("active_drain_gbps");
                transfer.remove("_active_drain_ns");
            }
        }
    }
    projected
}

#[test]
fn schema_parses_and_compiles() {
    let _published = compile_schema("metrics-schema.json");
    let _runtime = compile_schema("runtime-extensions-schema.json");
}

#[test]
fn captured_snapshots_validate_against_schema() {
    let dir = common::repo_root().join("fixtures/captures");
    let mut validated = 0usize;
    if dir.is_dir() {
        let published = compile_schema("metrics-schema.json");
        let runtime = compile_schema("runtime-extensions-schema.json");
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("read fixtures/captures")
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".snapshot.json"))
            })
            .collect();
        paths.sort();
        for path in paths {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).expect("read capture"))
                    .unwrap_or_else(|error| panic!("{} parses: {error}", path.display()));
            if let Err(error) = runtime.validate(&value) {
                panic!(
                    "{} violates the runtime extension contract: {error}",
                    path.display()
                );
            }
            let projected = published_projection(&value);
            if let Err(error) = published.validate(&projected) {
                panic!(
                    "{} violates the published contract after its documented runtime-only fields are projected out: {error}",
                    path.display()
                );
            }
            validated += 1;
        }
    }
    println!("conformance: {validated} captured snapshots present");
}

#[test]
fn result_fixtures_parse_with_expected_delta_schema() {
    let dir = common::fixtures_results_dir();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read fixtures/results")
        .map(|entry| entry.expect("dir entry").path())
        .collect();
    dirs.sort();
    let mut parsed = 0usize;
    let mut with_delta = 0usize;
    for entry in dirs {
        let path = entry.join("RESULT.json");
        if !path.is_file() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read RESULT.json"))
                .unwrap_or_else(|error| panic!("{} parses: {error}", path.display()));
        parsed += 1;
        if let Some(delta) = value.pointer("/muser/telemetry_delta") {
            assert_eq!(
                delta.get("schema").and_then(|v| v.as_str()),
                Some("muser.request-telemetry-delta.v1"),
                "{} telemetry_delta schema tag",
                path.display()
            );
            with_delta += 1;
        }
    }
    assert!(parsed > 0, "at least one RESULT.json fixture expected");
    println!(
        "conformance: {parsed} RESULT.json fixtures parsed, {with_delta} with telemetry_delta"
    );
}

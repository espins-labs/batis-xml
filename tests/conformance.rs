//! Conformance corpus runner — `fixtures/**/{name}.xml` paired with
//! `{name}.expected.json`.
//!
//! This corpus IS the portable spec: ports to other languages conform to
//! these pairs (tree-sitter corpus-test style), not to this codebase.
//!
//! `expected.json` files are never written by hand — they are generated
//! through a review-and-approve flow (insta-style) once the parser lands,
//! then locked against regressions.

use batis_xml::ParseResult;
use std::fs;
use std::path::Path;

fn run_dir(dir: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(dir);
    let mut checked = 0;
    for entry in fs::read_dir(&root).expect("fixture dir exists") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("xml") {
            continue;
        }
        let expected_path = path.with_extension("expected.json");
        if !expected_path.exists() {
            // Scaffold stage: input-only fixtures are skipped (expected
            // output is generated together with the implementation).
            continue;
        }
        let input = fs::read(&path).expect("read fixture xml");
        let expected: serde_json::Value =
            serde_json::from_slice(&fs::read(&expected_path).expect("read expected"))
                .expect("expected.json is valid json");
        let actual = serde_json::to_value(batis_xml::parse_bytes(&input)).expect("serialize");
        assert_eq!(actual, expected, "conformance mismatch: {}", path.display());
        checked += 1;
    }
    println!("{dir}: {checked} pairs checked");
    assert!(
        checked > 0,
        "no {dir} conformance pairs found — did the fixtures move?"
    );
}

/// A8 (cold code review): SqlText's manual Deserialize impl must round-trip
/// every real shape this crate's own flattening actually produces
/// (Variants/Union), across the whole conformance corpus -- not just the
/// hand-picked unit-test cases in model.rs. Serialize -> deserialize ->
/// re-serialize must reproduce the exact same JSON, i.e. no fixture's
/// SqlText should ever fall into the Unrecognized fallback.
#[test]
fn sql_text_round_trips_through_serde_across_the_whole_corpus() {
    let mut checked = 0;
    for dir in ["mybatis", "ibatis"] {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(dir);
        for entry in fs::read_dir(&root).expect("fixture dir exists") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("xml") {
                continue;
            }
            let input = fs::read(&path).expect("read fixture xml");
            let result = batis_xml::parse_bytes(&input);
            let json = serde_json::to_string(&result).expect("serialize");
            let round_tripped: ParseResult =
                serde_json::from_str(&json).expect("deserializes back");
            let json_again = serde_json::to_string(&round_tripped).expect("re-serialize");
            assert_eq!(
                json,
                json_again,
                "SqlText round-trip drifted for {}",
                path.display()
            );
            checked += 1;
        }
    }
    println!("sql_text round-trip: {checked} files checked");
    assert!(checked > 0, "no fixtures found for the round-trip check");
}

#[test]
fn conformance_mybatis() {
    run_dir("mybatis");
}

#[test]
fn conformance_ibatis() {
    run_dir("ibatis");
}

/// Contract: `detect_dialect` (MM-01-only cheap pre-check) must agree with
/// the full `parse_bytes`'s dialect for every file in the conformance
/// corpus -- the whole point of the cheap path is that callers can trust
/// it instead of paying for a full parse.
#[test]
fn detect_dialect_agrees_with_full_parse_across_corpus() {
    let mut checked = 0;
    for dir in ["mybatis", "ibatis"] {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(dir);
        for entry in fs::read_dir(&root).expect("fixture dir exists") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("xml") {
                continue;
            }
            let bytes = fs::read(&path).expect("read fixture xml");
            let full_dialect = batis_xml::parse_bytes(&bytes).dialect;
            let quick_dialect = batis_xml::detect_dialect(&bytes);
            assert_eq!(
                quick_dialect,
                full_dialect,
                "detect_dialect disagrees with parse_bytes for {}",
                path.display()
            );
            checked += 1;
        }
    }
    println!("detect_dialect contract: {checked} files checked");
    assert!(
        checked > 0,
        "no fixtures found to check the detect_dialect contract"
    );
}

/// The hostile set checks an invariant rather than conformance: parsing
/// must never panic.
#[test]
fn hostile_never_panics() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/hostile");
    for entry in fs::read_dir(&root).expect("hostile dir exists") {
        let path = entry.expect("dir entry").path();
        if path.is_file() && path.file_name().is_some_and(|n| n != ".gitkeep") {
            let bytes = fs::read(&path).expect("read hostile fixture");
            let _ = batis_xml::parse_bytes(&bytes); // any panic fails the test
        }
    }
}

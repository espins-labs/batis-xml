//! Conformance corpus runner — `fixtures/**/{name}.xml` paired with
//! `{name}.expected.json`.
//!
//! This corpus IS the portable spec: ports to other languages conform to
//! these pairs (tree-sitter corpus-test style), not to this codebase.
//!
//! `expected.json` files are never written by hand — they are generated
//! through a review-and-approve flow (insta-style) once the parser lands,
//! then locked against regressions.

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

#[test]
fn conformance_mybatis() {
    run_dir("mybatis");
}

#[test]
fn conformance_ibatis() {
    run_dir("ibatis");
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

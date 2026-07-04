//! Pins `schema/batis-xml.v1.json` against model drift. Feature-gated
//! (`schema`, off by default) since `schemars` is an optional, non-published
//! dependency — enabling it doesn't touch the default build or the wasm32
//! gate.
//!
//! On a legitimate model change: regenerate deliberately, review the diff,
//! then commit the updated schema. Field removals/renames are breaking per
//! `src/model.rs`'s doc comment and need explicit reviewer approval first.
#![cfg(feature = "schema")]

use batis_xml::ParseResult;

#[test]
fn schema_v1_matches_committed_snapshot() {
    let schema = schemars::schema_for!(ParseResult);
    let generated = serde_json::to_string_pretty(&schema).expect("schema serializes") + "\n";
    let committed_path = concat!(env!("CARGO_MANIFEST_DIR"), "/schema/batis-xml.v1.json");
    let committed =
        std::fs::read_to_string(committed_path).expect("schema/batis-xml.v1.json exists");
    assert_eq!(
        generated, committed,
        "generated JSON Schema for ParseResult drifted from schema/batis-xml.v1.json -- \
         regenerate deliberately, review the diff, then commit the updated schema"
    );
}

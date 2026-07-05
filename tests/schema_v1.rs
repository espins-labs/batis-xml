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
use schemars::schema::Schema;

/// B32 (cold code review, minor): `Statement`/`SqlFragment`/`ResultMap`'s
/// `span` field carries `#[serde(default)]` so this crate's *reader* can
/// still accept older JSON that predates the field (falls back to
/// `ByteSpan::default()`), but this crate's own *writer* always emits it
/// -- schemars, though, treats `#[serde(default)]` as disqualifying a
/// field from `required` unconditionally (`insert_object_property`'s
/// `!has_default && ...` check runs before any `#[schemars(required)]`
/// override is even consulted), so the generated schema described these
/// three fields as optional even though every document this crate
/// produces always has them. Patches the generated schema's `required`
/// list to match what this crate's writer actually guarantees, rather
/// than what schemars can infer purely from serde attributes.
fn patch_always_present_spans(schema: &mut schemars::schema::RootSchema) {
    for name in ["Statement", "SqlFragment", "ResultMap"] {
        let Some(Schema::Object(obj)) = schema.definitions.get_mut(name) else {
            panic!("expected an object schema definition for {name}");
        };
        let object_validation = obj
            .object
            .as_mut()
            .unwrap_or_else(|| panic!("expected object validation on {name}"));
        object_validation.required.insert("span".to_string());
    }
}

#[test]
fn schema_v1_matches_committed_snapshot() {
    let mut schema = schemars::schema_for!(ParseResult);
    patch_always_present_spans(&mut schema);
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

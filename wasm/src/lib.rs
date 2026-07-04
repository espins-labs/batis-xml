//! WebAssembly bindings for `batis-xml`. Minimal surface, JSON-string
//! boundary: consumers get the whole output model (schema v1, see
//! `../schema/batis-xml.v1.json`) as a JSON string rather than a
//! marshalled JS object -- simplest, schema-faithful, no per-field glue
//! code to keep in sync as the model grows.

use wasm_bindgen::prelude::*;

/// Parses mapper XML bytes and returns the `ParseResult` (schema v1) as a
/// JSON string. Never panics and never throws: encoding/parse failures
/// already surface as diagnostics inside the JSON per the core crate's
/// contract, and the (practically unreachable, since `ParseResult` has no
/// non-string map keys) serialization failure case falls back to the JSON
/// literal `null` rather than trapping the wasm instance.
#[wasm_bindgen]
pub fn parse(bytes: &[u8]) -> String {
    let result = batis_xml::parse_bytes(bytes);
    serde_json::to_string(&result).unwrap_or_else(|_| "null".to_string())
}

/// This crate's version, from `Cargo.toml`.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

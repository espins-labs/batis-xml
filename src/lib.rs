//! # batis-xml
//!
//! Parser and dynamic-SQL flattener for **MyBatis** and **iBatis** mapper XML.
//!
//! - Returns partial results plus diagnostics on broken legacy input —
//!   never panics, never returns `Err` (every anomaly is a [`Diagnostic`]).
//! - Flattens dynamic tags (`<if>`, `<choose>`, iBatis `<iterate>` /
//!   `<isNotEmpty>`, …) into concrete SQL shape candidates ([`SqlText`]).
//! - The serde serialization of the output model is a language-neutral
//!   schema: ports to other languages validate against the conformance
//!   corpus in `fixtures/` (input XML → expected JSON pairs), not against
//!   this codebase.
//! - Pure-Rust dependencies only — builds clean for `wasm32-unknown-unknown`.
//!
//! ## Status
//!
//! **Scaffold** — the public API and output model are final; the parser is
//! being built micro-feature-first with test-first development (unit tests
//! are prefixed with their spec id, e.g. `mm_01_...`).

mod encoding;
mod flatten;
mod model;
mod parse;
mod placeholder;

pub use model::*;

/// Parses an already-decoded string. Never fails — every anomaly is
/// reported through [`ParseResult::diagnostics`].
pub fn parse(source: &str) -> ParseResult {
    parse::parse_str(source)
}

/// Detects the encoding (UTF-8 / EUC-KR / CP949), decodes, then parses.
/// No `Err`: even encoding failures are absorbed as `mapper: None` plus
/// [`DiagCode::EncodingUndetectable`].
pub fn parse_bytes(bytes: &[u8]) -> ParseResult {
    let (source, mut diags) = encoding::decode(bytes);
    let mut result = parse::parse_str(&source);
    diags.append(&mut result.diagnostics);
    result.diagnostics = diags;
    result
}

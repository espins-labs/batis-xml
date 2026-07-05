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
//! MM-01 through MM-14 are complete and validated against a 195-file
//! real-world legacy mapper corpus (100% parse success; statement/binding
//! accuracy 98.9% MyBatis / 87.6% iBatis against an 85% bar). See the
//! crate's `README.md` for details.

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
    // Pre-decode byte cap (cold review B5): checking only post-decode (as
    // parse_str still does, defense-in-depth) means a huge input (e.g.
    // 1 GB) pays for a full decode and allocation before being rejected.
    if bytes.len() > parse::OVERSIZE_LIMIT {
        return oversize_result(bytes.len());
    }
    let (source, mut diags) = encoding::decode(bytes);
    let mut result = parse::parse_str(&source);
    diags.append(&mut result.diagnostics);
    result.diagnostics = diags;
    result
}

/// Cheap dialect pre-check: decodes (same encoding detection as
/// [`parse_bytes`]) and identifies the root element only — no statement/
/// fragment/resultMap capture, no dynamic-SQL flattening. Guaranteed to
/// agree with `parse_bytes(bytes).dialect` (see the contract test in
/// `tests/conformance.rs`), so callers can bucket files by dialect without
/// paying for a full parse first.
pub fn detect_dialect(bytes: &[u8]) -> Dialect {
    // Same pre-decode byte cap as parse_bytes (cold review B5) -- a "cheap
    // pre-check" that still decodes an arbitrarily huge input first isn't
    // cheap.
    if bytes.len() > parse::OVERSIZE_LIMIT {
        return Dialect::Unknown;
    }
    let (source, _diags) = encoding::decode(bytes);
    parse::detect_dialect_str(&source)
}

fn oversize_result(byte_len: usize) -> ParseResult {
    ParseResult {
        dialect: Dialect::Unknown,
        mapper: None,
        diagnostics: vec![Diagnostic {
            code: DiagCode::OversizeInput,
            span: None,
            message: format!(
                "input is {byte_len} bytes, over the {}-byte cap",
                parse::OVERSIZE_LIMIT
            ),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // B5 (cold code review): the oversize check must happen on the raw
    // byte length *before* decoding -- checking only after decoding means
    // a huge input still pays for a full decode/allocation first.

    #[test]
    fn parse_bytes_rejects_oversize_input_before_decoding() {
        let huge = vec![b'x'; parse::OVERSIZE_LIMIT + 1];
        let result = parse_bytes(&huge);
        assert_eq!(result.dialect, Dialect::Unknown);
        assert!(result.mapper.is_none());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, DiagCode::OversizeInput);
    }

    #[test]
    fn detect_dialect_rejects_oversize_input_before_decoding() {
        let huge = vec![b'x'; parse::OVERSIZE_LIMIT + 1];
        assert_eq!(detect_dialect(&huge), Dialect::Unknown);
    }

    #[test]
    fn parse_bytes_under_cap_is_unaffected() {
        let source = br#"<mapper namespace="x"><select id="a">SELECT 1</select></mapper>"#;
        let result = parse_bytes(source);
        assert_eq!(result.dialect, Dialect::Mybatis);
        assert!(result.diagnostics.is_empty());
    }
}

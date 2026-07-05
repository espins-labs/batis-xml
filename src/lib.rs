// B24 (cold code review): the crate doc IS the README, verbatim -- rather
// than maintaining a second, hand-written summary that can (and did) drift
// out of sync, and whose own code example was never compile-checked (a
// bare `?` in what rustdoc treats as `fn main()`, plus a `std::fs::read`
// that would fail at doctest run time since no such file exists). Making
// the README the single source of crate docs means `cargo test --doc`
// exercises its example directly.
#![doc = include_str!("../README.md")]

mod encoding;
mod flatten;
mod model;
mod parse;
mod placeholder;

pub use model::*;

/// Raw input byte cap (B25, cold code review: previously only documented
/// as "the 10 MB cap" in prose comments, not a checkable value). Input
/// over this size is rejected before decoding is even attempted — see
/// [`DiagCode::OversizeInput`], [`parse_bytes`], and [`detect_dialect`].
pub const MAX_INPUT_BYTES: usize = parse::OVERSIZE_LIMIT;

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
    if bytes.len() > MAX_INPUT_BYTES {
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
    if bytes.len() > MAX_INPUT_BYTES {
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
                MAX_INPUT_BYTES
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
        let huge = vec![b'x'; MAX_INPUT_BYTES + 1];
        let result = parse_bytes(&huge);
        assert_eq!(result.dialect, Dialect::Unknown);
        assert!(result.mapper.is_none());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, DiagCode::OversizeInput);
    }

    #[test]
    fn detect_dialect_rejects_oversize_input_before_decoding() {
        let huge = vec![b'x'; MAX_INPUT_BYTES + 1];
        assert_eq!(detect_dialect(&huge), Dialect::Unknown);
    }

    #[test]
    fn b25_max_input_bytes_is_public_and_matches_the_documented_ten_mib_cap() {
        // Cold code review B25: the cap was previously only a prose claim
        // ("the 10 MB cap") -- now a checkable public constant callers can
        // size their own pre-checks against without guessing.
        assert_eq!(MAX_INPUT_BYTES, 10 * 1024 * 1024);
    }

    #[test]
    fn parse_bytes_under_cap_is_unaffected() {
        let source = br#"<mapper namespace="x"><select id="a">SELECT 1</select></mapper>"#;
        let result = parse_bytes(source);
        assert_eq!(result.dialect, Dialect::Mybatis);
        assert!(result.diagnostics.is_empty());
    }
}

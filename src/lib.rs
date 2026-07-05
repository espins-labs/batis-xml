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

/// Detects the encoding via a BOM/declared-label-driven chain covering
/// every WHATWG encoding (not just UTF-8/EUC-KR/CP949 -- see `encoding.rs`
/// for the full chain), decodes, then parses. No `Err`: even encoding
/// failures are absorbed as `mapper: None` plus
/// [`DiagCode::EncodingUndetectable`]. [`ParseResult::encoding`] reports
/// which decoder was actually used.
pub fn parse_bytes(bytes: &[u8]) -> ParseResult {
    // Pre-decode byte cap (cold review B5): checking only post-decode (as
    // parse_str still does, defense-in-depth) means a huge input (e.g.
    // 1 GB) pays for a full decode and allocation before being rejected.
    if bytes.len() > MAX_INPUT_BYTES {
        return oversize_result(bytes.len());
    }
    let (source, mut diags, encoding) = encoding::decode(bytes);
    let mut result = parse::parse_str(&source);
    // A15 (cold code review, major model addition): parse_str always sets
    // this to "UTF-8" (correct for its own &str input, which is always
    // already-decoded UTF-8), but the *original* bytes may have been
    // decoded from something else entirely -- override with what the
    // detection chain actually used.
    result.encoding = Some(encoding.to_string());
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
    let (source, _diags, _encoding) = encoding::decode(bytes);
    parse::detect_dialect_str(&source)
}

fn oversize_result(byte_len: usize) -> ParseResult {
    ParseResult {
        dialect: Dialect::Unknown,
        mapper: None,
        // No decode was attempted at all -- the raw-byte cap rejected the
        // input before encoding.rs ever ran, so there's genuinely no
        // encoding to report (unlike every other ParseResult, which always
        // has one: see ParseResult::encoding's doc comment).
        encoding: None,
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

    // A15 (cold code review, major model addition): ParseResult.encoding
    // reports the WHATWG name of the decoder the detection chain actually
    // used, so a consumer can reproduce this crate's byte offsets by
    // decoding the *original* bytes the same way.

    #[test]
    fn a15_encoding_reports_utf8_for_plain_utf8_input() {
        let source = br#"<mapper namespace="x"><select id="a">SELECT 1</select></mapper>"#;
        let result = parse_bytes(source);
        assert_eq!(result.encoding.as_deref(), Some("UTF-8"));
    }

    #[test]
    fn a15_encoding_reports_euc_kr_for_euc_kr_input() {
        let (euckr_bytes, _, had_errors) = encoding_rs::EUC_KR.encode("그룹");
        assert!(!had_errors);
        let mut bytes = b"<?xml version=\"1.0\" encoding=\"EUC-KR\"?><mapper namespace=\"".to_vec();
        bytes.extend_from_slice(&euckr_bytes);
        bytes.extend_from_slice(b"\"></mapper>");

        let result = parse_bytes(&bytes);
        assert_eq!(result.encoding.as_deref(), Some("EUC-KR"));
    }

    #[test]
    fn a15_encoding_is_none_when_the_oversize_cap_rejects_before_any_decode() {
        // No decode was attempted at all -- unlike every other case, there
        // is genuinely no encoding to report here.
        let huge = vec![b'x'; MAX_INPUT_BYTES + 1];
        let result = parse_bytes(&huge);
        assert_eq!(result.encoding, None);
    }

    #[test]
    fn a15_encoding_is_utf8_for_the_str_based_parse_entry_point() {
        // parse() takes an already-decoded &str -- by Rust's own type
        // guarantee that's always UTF-8, regardless of what the original
        // bytes (if any) were encoded as before some other layer decoded
        // them into this &str.
        let result = parse(r#"<mapper namespace="x"></mapper>"#);
        assert_eq!(result.encoding.as_deref(), Some("UTF-8"));
    }

    #[test]
    fn a15_encoding_reports_utf16le_and_spans_are_relative_to_bom_stripped_content() {
        // BOM handling, explicit: a UTF-16LE document with a BOM decodes
        // with the BOM consumed (never appears in the decoded text), so
        // every span -- including the namespace attribute's -- is an
        // offset counted from right after the (now-vanished) BOM, not
        // from the start of the original file's raw bytes.
        let body = r#"<mapper namespace="x"></mapper>"#;
        let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
        for unit in body.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let result = parse_bytes(&bytes);
        assert_eq!(result.encoding.as_deref(), Some("UTF-16LE"));
        let mapper = result.mapper.expect("mapper root");
        let ns = mapper.namespace.expect("namespace");
        assert_eq!(&body[ns.span.start as usize..ns.span.end as usize], "x");
    }
}

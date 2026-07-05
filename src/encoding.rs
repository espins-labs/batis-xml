//! Encoding detection and decoding (MM-14), promoted to a global,
//! label-driven chain for 0.1.0 (code-atlas 95f5bb4, cold code review B14):
//!
//! 1. **BOM sniff**: a UTF-8 BOM is skipped; a UTF-16 LE/BE BOM selects the
//!    matching `encoding_rs` decoder directly. UTF-16 documents have no
//!    ASCII-safe way to expose their own `<?xml ... encoding="...">`
//!    declaration (the prolog isn't ASCII bytes at all), so the BOM is the
//!    only signal available for them.
//! 2. **UTF-8 attempt** (unchanged from before).
//! 3. **Declared label, trust-but-verify**: the XML declaration's
//!    `encoding` value is resolved via [`resolve_label`] (WHATWG's full
//!    label registry via `encoding_rs::Encoding::for_label`, covering
//!    Shift_JIS/EUC-JP/ISO-2022-JP, GBK/GB18030/Big5, Windows-125x/KOI8/
//!    ISO-8859-*, UTF-16, ... plus a small supplemental table for
//!    Windows/legacy Korean code-page names WHATWG doesn't register --
//!    see that function). If it resolves and decodes without errors, it's
//!    used directly. An unrecognized label or a failing decode both fall
//!    through to the next step -- reality wins either way.
//! 4. **EUC-KR heuristic fallback**: preserves the original MM-14 behavior
//!    for declaration-less Korean legacy mappers. A heuristic, not a
//!    capability limit -- disambiguating an *undeclared* Shift_JIS/GBK/etc.
//!    file is chardet-tier work and out of scope here.
//! 5. **Lossy + `EncodingUndetectable`** if nothing above worked.
//!
//! When the declared encoding disagrees with whichever of the above
//! actually succeeded, **reality wins** plus an `EncodingMismatch`
//! diagnostic -- [`declared_mismatch_diagnostic`] does this generically
//! (resolve the declared label, compare identity with the actual encoding
//! used) rather than a hardcoded per-family table, so it covers every
//! alias automatically.
//!
//! ## Span coarsening for re-encoded content
//!
//! Every `ByteSpan` elsewhere in this crate is computed by the XML reader
//! walking the *decoded* `String` this module returns — for UTF-8 input
//! (unchanged byte-for-byte by decoding) those offsets are exactly the
//! original bytes. For any document decoded from a non-UTF-8 encoding
//! (EUC-KR, Shift_JIS, GB18030, UTF-16, ...), decoding to UTF-8 changes
//! byte *widths* per character, so spans reported for such documents are
//! offsets into the *re-encoded* UTF-8 string, not the original raw bytes.
//! Building a byte-accurate original-to-UTF-8 offset map is out of scope
//! for M0 (the real-world corpus measured has zero non-UTF-8 files — see
//! spec 17) but is flagged here rather than silently claimed as exact.

use crate::model::{DiagCode, Diagnostic};

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
const UTF16LE_BOM: &[u8] = &[0xFF, 0xFE];
const UTF16BE_BOM: &[u8] = &[0xFE, 0xFF];

pub(crate) fn decode(bytes: &[u8]) -> (String, Vec<Diagnostic>) {
    // ① BOM sniff. UTF-16 LE/BE BOMs are checked first (2-byte prefixes,
    // and a UTF-16 document's declaration can't be read as ASCII anyway,
    // so there's nothing else to check against here).
    if let Some(rest) = bytes.strip_prefix(UTF16LE_BOM) {
        let (decoded, _, had_errors) = encoding_rs::UTF_16LE.decode(rest);
        if !had_errors {
            return (decoded.into_owned(), Vec::new());
        }
    } else if let Some(rest) = bytes.strip_prefix(UTF16BE_BOM) {
        let (decoded, _, had_errors) = encoding_rs::UTF_16BE.decode(rest);
        if !had_errors {
            return (decoded.into_owned(), Vec::new());
        }
    }

    // A UTF-8 BOM is stripped before anything else below, so the
    // declaration scan doesn't fail to find "<?xml..." right after it
    // (str::trim_start only strips whitespace, not U+FEFF) and the
    // returned string never carries a leading U+FEFF either.
    let content = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    let declared = detect_declared_encoding(content);

    // ② UTF-8 attempt (unchanged).
    if let Ok(s) = std::str::from_utf8(content) {
        let diagnostics = declared_mismatch_diagnostic(declared.as_deref(), encoding_rs::UTF_8)
            .into_iter()
            .collect();
        return (s.to_string(), diagnostics);
    }

    // ③ Declared label, trust-but-verify.
    if let Some(label) = &declared {
        if let Some(enc) = resolve_label(label) {
            let (decoded, _, had_errors) = enc.decode(content);
            if !had_errors {
                let diagnostics = declared_mismatch_diagnostic(declared.as_deref(), enc)
                    .into_iter()
                    .collect();
                return (decoded.into_owned(), diagnostics);
            }
        }
    }

    // ④ EUC-KR heuristic fallback.
    let (decoded, _, had_errors) = encoding_rs::EUC_KR.decode(content);
    if !had_errors {
        let diagnostics = declared_mismatch_diagnostic(declared.as_deref(), encoding_rs::EUC_KR)
            .into_iter()
            .collect();
        return (decoded.into_owned(), diagnostics);
    }

    // ⑤ Lossy + EncodingUndetectable.
    (
        String::from_utf8_lossy(content).into_owned(),
        vec![Diagnostic {
            code: DiagCode::EncodingUndetectable,
            span: None,
            message:
                "could not confidently detect an encoding (tried UTF-8, the declared label, EUC-KR/CP949 heuristic); decoded lossily"
                    .to_string(),
        }],
    )
}

/// Resolves a declared encoding label to its `encoding_rs` encoding.
/// Tries the WHATWG-standard label registry first
/// (`encoding_rs::Encoding::for_label`) -- this alone covers every
/// standard alias for every encoding `encoding_rs` implements (Shift_JIS,
/// GB18030/GBK, Big5, UTF-16, EUC-KR's own WHATWG-recognized aliases like
/// `windows-949`, ...). Falls back to a small supplemental table for
/// Windows/legacy code-page names for Korean text that WHATWG doesn't
/// register at all (`CP949`, `MS949`, `x-euc-kr`, `UHC`) but which are
/// common in real declared-encoding attributes from Windows-authored
/// legacy tooling -- all of them the same `encoding_rs::EUC_KR` encoding
/// in practice. (This is the one piece of the old hardcoded family table
/// that `for_label` alone doesn't reconstruct.)
fn resolve_label(label: &str) -> Option<&'static encoding_rs::Encoding> {
    if let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes()) {
        return Some(enc);
    }
    let normalized: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    match normalized.as_str() {
        "CP949" | "MS949" | "XEUCKR" | "UHC" => Some(encoding_rs::EUC_KR),
        _ => None,
    }
}

/// Compares the XML declaration's `encoding` (if any) against the `actual`
/// encoding reality settled on, returning a mismatch diagnostic when they
/// disagree (including when the declared label doesn't resolve to any
/// known encoding at all). Reality always wins — the caller has already
/// picked (and returns) the actually-successful decode regardless of what
/// this reports.
fn declared_mismatch_diagnostic(
    declared: Option<&str>,
    actual: &'static encoding_rs::Encoding,
) -> Option<Diagnostic> {
    let declared = declared?;
    if resolve_label(declared) == Some(actual) {
        return None;
    }
    Some(Diagnostic {
        code: DiagCode::EncodingMismatch,
        span: None,
        message: format!(
            "declared encoding '{declared}' does not match the actual encoding ({}); using {}",
            actual.name(),
            actual.name()
        ),
    })
}

/// Scans the first 200 raw bytes for `<?xml ... encoding="..." ?>`. A
/// plain byte-level scan, not a full XML parse — the prolog is always
/// ASCII-only per the XML spec, so `from_utf8_lossy` on this prefix is
/// safe and exact even when the rest of the document is in some other
/// ASCII-compatible encoding (whose ASCII-range bytes are identical to
/// ASCII/UTF-8 anyway). Not meaningful for UTF-16 input (handled entirely
/// by the BOM sniff before this is ever called).
fn detect_declared_encoding(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(200)];
    let text = String::from_utf8_lossy(head);
    if !text.trim_start().starts_with("<?xml") {
        return None;
    }
    let needle = "encoding=";
    let pos = text.find(needle)?;
    let rest = &text[pos + needle.len()..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DiagCode;

    fn utf16le_bytes(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in s.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    #[test]
    fn mm_14_valid_utf8_no_declaration_decodes_cleanly() {
        let bytes = "<mapper namespace=\"x\"></mapper>".as_bytes();
        let (source, diagnostics) = decode(bytes);
        assert_eq!(source, "<mapper namespace=\"x\"></mapper>");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn mm_14_utf8_declared_and_actual_agree() {
        let bytes =
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><mapper namespace=\"x\"/>".as_bytes();
        let (_, diagnostics) = decode(bytes);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn mm_14_euckr_bytes_decode_and_declared_matches() {
        // "그룹" (group) — genuine EUC-KR bytes, not UTF-8.
        let (euckr_bytes, _, had_errors) = encoding_rs::EUC_KR.encode("그룹");
        assert!(!had_errors);
        let mut bytes = b"<?xml version=\"1.0\" encoding=\"EUC-KR\"?><mapper namespace=\"".to_vec();
        bytes.extend_from_slice(&euckr_bytes);
        bytes.extend_from_slice(b"\"></mapper>");

        let (source, diagnostics) = decode(&bytes);
        assert!(source.contains("그룹"));
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn mm_14_declared_utf8_but_actual_euckr_reality_wins_with_mismatch() {
        let (euckr_bytes, _, _) = encoding_rs::EUC_KR.encode("그룹");
        let mut bytes = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><mapper namespace=\"".to_vec();
        bytes.extend_from_slice(&euckr_bytes);
        bytes.extend_from_slice(b"\"></mapper>");

        let (source, diagnostics) = decode(&bytes);
        assert!(source.contains("그룹")); // reality (EUC-KR) wins, not the false UTF-8 claim
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagCode::EncodingMismatch);
    }

    #[test]
    fn mm_14_declared_euckr_but_actual_utf8_reality_wins_with_mismatch() {
        let bytes =
            "<?xml version=\"1.0\" encoding=\"EUC-KR\"?><mapper namespace=\"그룹\"></mapper>"
                .as_bytes();
        let (source, diagnostics) = decode(bytes);
        assert!(source.contains("그룹")); // reality (UTF-8) wins
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagCode::EncodingMismatch);
    }

    #[test]
    fn mm_14_declared_cp949_matches_euckr_family_no_mismatch() {
        let (euckr_bytes, _, _) = encoding_rs::EUC_KR.encode("그룹");
        let mut bytes = b"<?xml version=\"1.0\" encoding=\"CP949\"?><mapper namespace=\"".to_vec();
        bytes.extend_from_slice(&euckr_bytes);
        bytes.extend_from_slice(b"\"></mapper>");

        let (_, diagnostics) = decode(&bytes);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn mm_14_undetectable_bytes_decode_lossily_with_diagnostic() {
        // Invalid in both UTF-8 and EUC-KR: a lone continuation-style byte
        // that EUC-KR's decoder also rejects at that position.
        let bytes: &[u8] = &[
            0xFF, 0xFE, 0xFD, b'<', b'a', b'>', b'x', b'<', b'/', b'a', b'>',
        ];
        let (_, diagnostics) = decode(bytes);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagCode::EncodingUndetectable);
    }

    // --- B14 (cold code review): global, label-driven encoding chain.
    // Per-family synthetic matrix: declared-and-matching (clean) and
    // declared-but-different (reality wins + EncodingMismatch), plus
    // UTF-16LE-with-BOM and an unknown-label case. All mm_14_* tests
    // above stay green unmodified.

    #[test]
    fn shift_jis_declared_and_actual_agree() {
        let (sjis_bytes, _, had_errors) = encoding_rs::SHIFT_JIS.encode("テスト");
        assert!(!had_errors);
        let mut bytes =
            b"<?xml version=\"1.0\" encoding=\"Shift_JIS\"?><mapper namespace=\"".to_vec();
        bytes.extend_from_slice(&sjis_bytes);
        bytes.extend_from_slice(b"\"></mapper>");

        let (source, diagnostics) = decode(&bytes);
        assert!(source.contains("テスト"));
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn declared_shift_jis_but_actual_utf8_reality_wins_with_mismatch() {
        let bytes =
            "<?xml version=\"1.0\" encoding=\"Shift_JIS\"?><mapper namespace=\"テスト\"></mapper>"
                .as_bytes();
        let (source, diagnostics) = decode(bytes);
        assert!(source.contains("テスト")); // reality (UTF-8) wins
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagCode::EncodingMismatch);
    }

    #[test]
    fn gb18030_declared_and_actual_agree() {
        let (gb_bytes, _, had_errors) = encoding_rs::GB18030.encode("测试");
        assert!(!had_errors);
        let mut bytes =
            b"<?xml version=\"1.0\" encoding=\"GB18030\"?><mapper namespace=\"".to_vec();
        bytes.extend_from_slice(&gb_bytes);
        bytes.extend_from_slice(b"\"></mapper>");

        let (source, diagnostics) = decode(&bytes);
        assert!(source.contains("测试"));
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn declared_gb18030_but_actual_utf8_reality_wins_with_mismatch() {
        let bytes =
            "<?xml version=\"1.0\" encoding=\"GB18030\"?><mapper namespace=\"测试\"></mapper>"
                .as_bytes();
        let (source, diagnostics) = decode(bytes);
        assert!(source.contains("测试")); // reality (UTF-8) wins
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagCode::EncodingMismatch);
    }

    #[test]
    fn big5_declared_and_actual_agree() {
        let (big5_bytes, _, had_errors) = encoding_rs::BIG5.encode("測試");
        assert!(!had_errors);
        let mut bytes = b"<?xml version=\"1.0\" encoding=\"Big5\"?><mapper namespace=\"".to_vec();
        bytes.extend_from_slice(&big5_bytes);
        bytes.extend_from_slice(b"\"></mapper>");

        let (source, diagnostics) = decode(&bytes);
        assert!(source.contains("測試"));
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn utf16le_with_bom_decodes_cleanly() {
        let body = r#"<mapper namespace="x"><select id="a">SELECT 1</select></mapper>"#;
        let mut bytes = UTF16LE_BOM.to_vec();
        bytes.extend_from_slice(&utf16le_bytes(body));

        let (source, diagnostics) = decode(&bytes);
        assert_eq!(source, body);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn utf16be_with_bom_decodes_cleanly() {
        let body = r#"<mapper namespace="x"><select id="a">SELECT 1</select></mapper>"#;
        let mut bytes = UTF16BE_BOM.to_vec();
        for unit in body.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }

        let (source, diagnostics) = decode(&bytes);
        assert_eq!(source, body);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn unknown_declared_label_falls_through_with_mismatch_diagnostic() {
        let bytes = br#"<?xml version="1.0" encoding="totally-not-a-real-encoding"?><mapper namespace="x"></mapper>"#;
        let (source, diagnostics) = decode(bytes);
        assert!(source.contains("<mapper"));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagCode::EncodingMismatch);
    }

    #[test]
    fn utf8_bom_before_xml_declaration_no_longer_defeats_declared_encoding_detection() {
        // B8 (cold code review): a UTF-8 BOM before the XML declaration
        // used to make detect_declared_encoding's "<?xml" prefix check
        // fail (str::trim_start doesn't strip U+FEFF), silently losing
        // the declared label -- and with it, any mismatch diagnostic --
        // even though the actual content plainly disagreed with it.
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(
            b"<?xml version=\"1.0\" encoding=\"EUC-KR\"?><mapper namespace=\"x\"></mapper>",
        );
        let (source, diagnostics) = decode(&bytes);
        assert!(source.starts_with("<?xml")); // BOM gone, declaration intact
        assert!(source.contains("<mapper"));
        assert!(!source.contains('\u{FEFF}')); // the BOM itself doesn't leak into the decoded string
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagCode::EncodingMismatch);
    }
}

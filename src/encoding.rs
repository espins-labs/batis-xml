//! Encoding detection and decoding (MM-14).
//!
//! UTF-8 first; on failure try EUC-KR/CP949 (`encoding_rs::EUC_KR` already
//! implements the CP949-compatible superset, so one decoder covers both
//! declared names). When the XML declaration's `encoding` disagrees with
//! reality, **reality wins** plus an `EncodingMismatch` diagnostic. If
//! neither decodes cleanly, decode UTF-8 lossily plus
//! `EncodingUndetectable`.
//!
//! ## Span coarsening for re-encoded content
//!
//! Every `ByteSpan` elsewhere in this crate is computed by the XML reader
//! walking the *decoded* `String` this module returns — for UTF-8 input
//! (unchanged byte-for-byte by decoding) those offsets are exactly the
//! original bytes. For EUC-KR/CP949 input, decoding to UTF-8 changes byte
//! *widths* per character (e.g. a 2-byte EUC-KR Hangul syllable becomes 3
//! bytes in UTF-8), so spans reported for such documents are offsets into
//! the *re-encoded* UTF-8 string, not the original raw bytes. Building a
//! byte-accurate original-to-UTF-8 offset map is out of scope for M0 (the
//! real-world corpus measured has zero EUC-KR files — see spec 17) but
//! is flagged here rather than silently claimed as exact.

use crate::model::{DiagCode, Diagnostic};

pub(crate) fn decode(bytes: &[u8]) -> (String, Vec<Diagnostic>) {
    let declared = detect_declared_encoding(bytes);

    if let Ok(s) = std::str::from_utf8(bytes) {
        let diagnostics = mismatch_diagnostic(declared.as_deref(), "UTF-8")
            .into_iter()
            .collect();
        return (s.to_string(), diagnostics);
    }

    let (decoded, _, had_errors) = encoding_rs::EUC_KR.decode(bytes);
    if !had_errors {
        let diagnostics = mismatch_diagnostic(declared.as_deref(), "EUC-KR")
            .into_iter()
            .collect();
        return (decoded.into_owned(), diagnostics);
    }

    let lossy = String::from_utf8_lossy(bytes).into_owned();
    (
        lossy,
        vec![Diagnostic {
            code: DiagCode::EncodingUndetectable,
            span: None,
            message:
                "could not confidently detect an encoding (tried UTF-8, EUC-KR/CP949); decoded lossily"
                    .to_string(),
        }],
    )
}

/// Compares the XML declaration's `encoding` (if any) against the
/// encoding reality actually settled on, returning a mismatch diagnostic
/// when they disagree. Reality always wins — the caller has already
/// picked (and returns) the actually-successful decode regardless of what
/// this reports.
fn mismatch_diagnostic(declared: Option<&str>, actual_family: &str) -> Option<Diagnostic> {
    let declared = declared?;
    if encoding_family_matches(declared, actual_family) {
        return None;
    }
    Some(Diagnostic {
        code: DiagCode::EncodingMismatch,
        span: None,
        message: format!(
            "declared encoding '{declared}' does not match the actual encoding ({actual_family}); using {actual_family}"
        ),
    })
}

fn encoding_family_matches(declared: &str, actual_family: &str) -> bool {
    let normalized: String = declared
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    match actual_family {
        "UTF-8" => normalized == "UTF8",
        "EUC-KR" => matches!(
            normalized.as_str(),
            "EUCKR" | "CP949" | "MS949" | "XEUCKR" | "UHC"
        ),
        _ => false,
    }
}

/// Scans the first 200 raw bytes for `<?xml ... encoding="..." ?>`. A
/// plain byte-level scan, not a full XML parse — the prolog is always
/// ASCII-only per the XML spec, so `from_utf8_lossy` on this prefix is
/// safe and exact even when the rest of the document is EUC-KR (whose
/// ASCII-range bytes are identical to ASCII/UTF-8 anyway).
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
}

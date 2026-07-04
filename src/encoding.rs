//! Encoding detection and decoding (MM-14).
//!
//! UTF-8 first; on failure try EUC-KR/CP949. When the XML declaration's
//! `encoding` disagrees with reality, **reality wins** plus an
//! `EncodingMismatch` diagnostic. If detection fails entirely, decode
//! lossily plus `EncodingUndetectable`.
//!
//! Note: every `ByteSpan` is defined against the **original bytes**, so
//! when decoding changes byte lengths (EUC-KR → UTF-8) span computation
//! must track the original byte stream. (Not required for M0 — the target
//! corpora measured so far are UTF-8/ASCII.)

use crate::model::Diagnostic;

/// TODO(MM-14): current stub performs lossy UTF-8 decoding only.
pub(crate) fn decode(bytes: &[u8]) -> (String, Vec<Diagnostic>) {
    (String::from_utf8_lossy(bytes).into_owned(), Vec::new())
}

#[cfg(test)]
mod tests {
    // mm_14_* table tests (synthetic EUC-KR fixtures, declared-vs-actual
    // mismatch).
}

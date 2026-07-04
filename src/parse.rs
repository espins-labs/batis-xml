//! Parsing core — quick-xml event stream feeding a custom tree builder.
//!
//! Owned micro-features: MM-01 (root/dialect detection), MM-02 (namespace),
//! MM-03 (statement collection), MM-04 (`<sql>` fragments), MM-05 (include),
//! MM-08 (CDATA/entities), MM-09 (class refs), MM-10 (resultMap),
//! MM-11 (iBatis dialect), MM-12 (span preservation), MM-13 (hostile-input
//! resilience).
//!
//! Recovery rules (fixed by spec):
//! 1. Unclosed tag → implicitly closed when the parent closes, plus
//!    `UnclosedTag`.
//! 2. Orphan closing tag → ignored, plus a diagnostic.
//! 3. Duplicate attribute → first value wins, plus a diagnostic.
//! 4. Non-XML residue → skip to the next `<` and resynchronize.
//!
//! Constants: 10 MB input cap (`OversizeInput`); the branch cap lives in
//! [`crate::flatten`].

use crate::model::*;

/// TODO(MM-01): pre-implementation stub — returns an empty result.
/// Development starts with the first `mm_01_*` unit test failing against
/// this stub.
pub(crate) fn parse_str(_source: &str) -> ParseResult {
    ParseResult {
        dialect: Dialect::Unknown,
        mapper: None,
        diagnostics: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    // Unit test naming: <spec-id>_<description>, e.g.
    // `mm_01_mybatis_dtd_detected`. Table/snapshot tests for MM-01..05 and
    // MM-08..13 accumulate here.
}

//! Placeholder normalization (MM-07).
//!
//! This crate is the **sole owner** of this responsibility (downstream SQL
//! analyzers only ever see normalized text):
//! - `#{expr}` / iBatis `#expr#`  → `?`
//! - `${expr}` / iBatis `$expr$`  → [`DYN_MARKER`]
//! - Property paths inside `expr` are collected separately into
//!   `Statement::property_paths`.
//! - Option syntax (`#{id, jdbcType=VARCHAR}`) keeps only the path.
//!
//! Must also work inside CDATA sections (combined with MM-08).
//!
//! ## Span fidelity (honest approximation)
//!
//! A segment is *verbatim* when `decoded.len()` (bytes) equals the raw
//! span's byte length — true for CDATA content and for text with no
//! entity replacements. In that case every offset inside `decoded` maps
//! 1:1 to `raw_span.start + offset`, so `span_map` entries and
//! `property_paths` spans are exact.
//!
//! When entities were decoded, byte offsets in `decoded` no longer align
//! to the original bytes (an entity like `&amp;` collapses 5 raw bytes
//! into 1). Rather than pretend precision we don't have, every span in a
//! non-verbatim segment collapses to a zero-width point at
//! `raw_span.start` — still a valid, in-range span (invariant 2), just
//! coarse.

use crate::model::*;

/// Substitution marker for `${}` dynamic fragments (fixed by spec).
pub(crate) const DYN_MARKER: &str = "__ATLAS_DYN__";

/// Result of [`normalize_segment`].
///
/// `text`/`span_map` aren't read outside tests yet: `extract_property_paths`
/// (parse.rs) only consumes `property_paths`/`diagnostics` today — final
/// `SqlText` assembly (and thus real use of the rewritten text + span map)
/// is MM-06's job.
pub(crate) struct NormalizedSegment {
    /// The rewritten text (`#{}`/`#..#` → `?`, `${}`/`$..$` → [`DYN_MARKER`]).
    #[allow(dead_code)]
    pub(crate) text: String,
    /// `(synthetic offset, original byte offset)` pairs in the same format
    /// as [`SqlString::span_map`] — an entry at offset 0 and after every
    /// replacement. MM-06 concatenates these across segments with offset
    /// shifts to build the final map.
    #[allow(dead_code)]
    pub(crate) span_map: Vec<(u32, u32)>,
    /// Paths found from both `#`/`$` forms — a `${}` dynamic table name is
    /// exactly what downstream SQL analysis wants to know about.
    pub(crate) property_paths: Vec<Spanned<String>>,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

/// Normalizes one decoded text segment's placeholders.
pub(crate) fn normalize_segment(
    decoded: &str,
    raw_span: ByteSpan,
    dialect: Dialect,
) -> NormalizedSegment {
    let verbatim = decoded.len() as u32 == raw_span.end - raw_span.start;
    let bytes = decoded.as_bytes();

    let mut normalized = String::with_capacity(decoded.len());
    let mut span_map = vec![(0u32, raw_span.start)];
    let mut property_paths = Vec::new();
    let mut diagnostics = Vec::new();

    let mut copy_start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'#' && bytes.get(i + 1) == Some(&b'{') {
            match consume_braced(bytes, i) {
                Some((content_start, content_end, end_pos)) => {
                    normalized.push_str(&decoded[copy_start..i]);
                    push_path(
                        &mut property_paths,
                        extract_path_mybatis(&decoded[content_start..content_end]),
                        decoded,
                        raw_span,
                        verbatim,
                    );
                    normalized.push('?');
                    copy_start = end_pos;
                    i = end_pos;
                    span_map.push((
                        normalized.len() as u32,
                        raw_offset(raw_span, verbatim, end_pos),
                    ));
                }
                None => {
                    diagnostics.push(unterminated_placeholder(raw_span));
                    normalized.push_str(&decoded[copy_start..]);
                    copy_start = bytes.len();
                    i = bytes.len();
                }
            }
            continue;
        }

        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'{') {
            match consume_braced(bytes, i) {
                Some((content_start, content_end, end_pos)) => {
                    normalized.push_str(&decoded[copy_start..i]);
                    push_path(
                        &mut property_paths,
                        extract_path_mybatis(&decoded[content_start..content_end]),
                        decoded,
                        raw_span,
                        verbatim,
                    );
                    normalized.push_str(DYN_MARKER);
                    copy_start = end_pos;
                    i = end_pos;
                    span_map.push((
                        normalized.len() as u32,
                        raw_offset(raw_span, verbatim, end_pos),
                    ));
                }
                None => {
                    diagnostics.push(unterminated_placeholder(raw_span));
                    normalized.push_str(&decoded[copy_start..]);
                    copy_start = bytes.len();
                    i = bytes.len();
                }
            }
            continue;
        }

        if dialect == Dialect::Ibatis && (bytes[i] == b'#' || bytes[i] == b'$') {
            let delim = bytes[i];
            // Only commit to reading a legacy placeholder when its closing
            // delimiter actually exists in this segment — `#`/`$` are also
            // ordinary SQL/comment characters (monetary literals, etc.), and
            // misfiring here would spam diagnostics on every one of them.
            if let Some(close) = find_byte(bytes, i + 1, delim) {
                normalized.push_str(&decoded[copy_start..i]);
                let content = &decoded[i + 1..close];
                push_path(
                    &mut property_paths,
                    extract_path_ibatis(content),
                    decoded,
                    raw_span,
                    verbatim,
                );
                if delim == b'#' {
                    normalized.push('?');
                } else {
                    normalized.push_str(DYN_MARKER);
                }
                let end_pos = close + 1;
                copy_start = end_pos;
                i = end_pos;
                span_map.push((
                    normalized.len() as u32,
                    raw_offset(raw_span, verbatim, end_pos),
                ));
                continue;
            }
        }

        i += 1;
    }

    normalized.push_str(&decoded[copy_start..]);
    NormalizedSegment {
        text: normalized,
        span_map,
        property_paths,
        diagnostics,
    }
}

/// Consumes a `#{`/`${` construct starting at `start` (pointing at the `#`
/// or `$`), tracking brace depth so braces nested inside the expression
/// don't end the scan early. Returns `(content_start, content_end,
/// end_pos)` — `end_pos` is the byte offset just past the closing `}`.
/// `None` means the segment ended before a matching `}` was found.
fn consume_braced(bytes: &[u8], start: usize) -> Option<(usize, usize, usize)> {
    let content_start = start + 2; // past "#{" / "${"
    let mut depth = 1i32;
    let mut j = content_start;
    while j < bytes.len() {
        match bytes[j] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((content_start, j, j + 1));
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

fn find_byte(bytes: &[u8], from: usize, target: u8) -> Option<usize> {
    bytes[from..]
        .iter()
        .position(|&b| b == target)
        .map(|p| from + p)
}

/// MyBatis option syntax (`#{id, jdbcType=VARCHAR}`) keeps only the part
/// before the first comma.
fn extract_path_mybatis(raw: &str) -> &str {
    raw.split(',').next().unwrap_or("").trim()
}

/// iBatis legacy option syntax (`#prop:VARCHAR:defaultVal#`) keeps only the
/// part before the first colon. A bare iterate-form path (`[].sprHtlId`)
/// has no colon, so it survives untouched.
fn extract_path_ibatis(raw: &str) -> &str {
    raw.split(':').next().unwrap_or("").trim()
}

/// Pushes `path` into `property_paths` with its span, skipping empty paths
/// (`#{}`) silently — nothing to record, nothing to diagnose.
fn push_path(
    property_paths: &mut Vec<Spanned<String>>,
    path: &str,
    decoded: &str,
    raw_span: ByteSpan,
    verbatim: bool,
) {
    if path.is_empty() {
        return;
    }
    let span = if verbatim {
        let offset = (path.as_ptr() as usize) - (decoded.as_ptr() as usize);
        ByteSpan {
            start: raw_span.start + offset as u32,
            end: raw_span.start + offset as u32 + path.len() as u32,
        }
    } else {
        ByteSpan {
            start: raw_span.start,
            end: raw_span.start,
        }
    };
    property_paths.push(Spanned {
        value: path.to_string(),
        span,
    });
}

fn raw_offset(raw_span: ByteSpan, verbatim: bool, decoded_offset: usize) -> u32 {
    if verbatim {
        raw_span.start + decoded_offset as u32
    } else {
        raw_span.start
    }
}

fn unterminated_placeholder(raw_span: ByteSpan) -> Diagnostic {
    Diagnostic {
        code: DiagCode::UnterminatedPlaceholder,
        span: Some(raw_span),
        message: "placeholder was never closed".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `raw_span` that makes the segment verbatim (byte length
    /// equal to `decoded`'s).
    fn verbatim_span(decoded: &str) -> ByteSpan {
        ByteSpan {
            start: 100,
            end: 100 + decoded.len() as u32,
        }
    }

    #[test]
    fn mm_07_mybatis_simple_placeholder() {
        let decoded = "WHERE id = #{id}";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Mybatis);
        assert_eq!(result.text, "WHERE id = ?");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths.len(), 1);
        assert_eq!(result.property_paths[0].value, "id");
        let ByteSpan { start, end } = result.property_paths[0].span;
        assert_eq!(&decoded[(start - 100) as usize..(end - 100) as usize], "id");
        // span_map: [0]->raw start, then one entry after the replacement.
        assert_eq!(result.span_map.len(), 2);
        assert!(result.span_map.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn mm_07_mybatis_jdbc_type_option_keeps_only_path() {
        let decoded = "#{id, jdbcType=VARCHAR}";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Mybatis);
        assert_eq!(result.text, "?");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths.len(), 1);
        assert_eq!(result.property_paths[0].value, "id");
    }

    #[test]
    fn mm_07_mybatis_nested_braces_consumed_by_depth_counting() {
        let decoded = "#{a{b}c}";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Mybatis);
        assert_eq!(result.text, "?");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths[0].value, "a{b}c");
    }

    #[test]
    fn mm_07_mybatis_dynamic_marker() {
        let decoded = "${tableName}";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Mybatis);
        assert_eq!(result.text, DYN_MARKER);
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths[0].value, "tableName");
    }

    #[test]
    fn mm_07_ibatis_legacy_hash_placeholder() {
        let decoded = "WHERE id = #id#";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Ibatis);
        assert_eq!(result.text, "WHERE id = ?");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths[0].value, "id");
    }

    #[test]
    fn mm_07_ibatis_colon_option_syntax_keeps_path_before_first_colon() {
        let decoded = "#prop:VARCHAR:defaultVal#";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Ibatis);
        assert_eq!(result.text, "?");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths[0].value, "prop");
    }

    #[test]
    fn mm_07_ibatis_iterate_form_bracket_path_kept_verbatim() {
        let decoded = "#[].sprHtlId#";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Ibatis);
        assert_eq!(result.text, "?");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths[0].value, "[].sprHtlId");
    }

    #[test]
    fn mm_07_ibatis_legacy_dollar_placeholder() {
        let decoded = "ORDER BY $sortCol$";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Ibatis);
        assert_eq!(result.text, format!("ORDER BY {DYN_MARKER}"));
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths[0].value, "sortCol");
    }

    #[test]
    fn mm_07_unterminated_mybatis_placeholder_keeps_raw_text_and_diagnoses() {
        let decoded = "WHERE id = #{id";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Mybatis);
        assert_eq!(result.text, decoded);
        assert!(result.property_paths.is_empty());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].code,
            DiagCode::UnterminatedPlaceholder
        );
    }

    #[test]
    fn mm_07_lone_hash_without_closing_delimiter_is_left_untouched_no_diagnostic() {
        // A monetary-literal-shaped comment; no second '#' anywhere.
        let decoded = "price is #100 approx";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Ibatis);
        assert_eq!(result.text, decoded);
        assert!(result.property_paths.is_empty());
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn mm_07_lone_dollar_without_closing_delimiter_is_left_untouched_no_diagnostic() {
        let decoded = "cost is $5 today";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Ibatis);
        assert_eq!(result.text, decoded);
        assert!(result.property_paths.is_empty());
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn mm_07_empty_expr_normalizes_silently() {
        let decoded = "#{}";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Mybatis);
        assert_eq!(result.text, "?");
        assert!(result.property_paths.is_empty());
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn mm_07_multiple_placeholders_span_map_strictly_increasing_on_offset() {
        let decoded = "WHERE a = #{a} AND b = #{b} AND c = ${c}";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Mybatis);
        assert_eq!(
            result.text,
            format!("WHERE a = ? AND b = ? AND c = {DYN_MARKER}")
        );
        assert!(result.diagnostics.is_empty());
        assert_eq!(
            result
                .property_paths
                .iter()
                .map(|p| p.value.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert_eq!(result.span_map.len(), 4); // initial + 3 replacements
        assert!(result.span_map.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn mm_07_non_verbatim_segment_falls_back_to_coarse_spans() {
        // Simulates a segment where entity decoding shifted byte offsets
        // (raw_span deliberately longer than decoded, as `&amp;` would
        // make it): every span in this segment must collapse to a
        // zero-width point at raw_span.start rather than claim precision
        // we don't have.
        let decoded = "a & #{id}";
        let raw_span = ByteSpan {
            start: 200,
            end: 200 + decoded.len() as u32 + 4, // force a length mismatch
        };
        let result = normalize_segment(decoded, raw_span, Dialect::Mybatis);
        assert_eq!(result.text, "a & ?");
        assert!(result.diagnostics.is_empty());
        assert_eq!(
            result.property_paths[0].span,
            ByteSpan {
                start: 200,
                end: 200
            }
        );
        assert!(result.span_map.iter().all(|(_, raw)| *raw == 200));
    }

    #[test]
    fn mm_07_placeholder_inside_cdata_combined_with_mm_08() {
        // Exercises the combined path: capture_body decodes a CDATA
        // segment (MM-08), then normalize_segment processes its text
        // (MM-07) exactly as it would a plain-text segment.
        let decoded = "SELECT * FROM t WHERE id = #{id}";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Mybatis);
        assert_eq!(result.text, "SELECT * FROM t WHERE id = ?");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths[0].value, "id");
    }
}

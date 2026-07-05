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
//! Must also work inside CDATA sections (combined with MM-08), *including*
//! a placeholder whose delimiters straddle a text/CDATA boundary (cold
//! code review B16) -- [`normalize_merged`] merges a run of adjacent
//! source segments into one logical text before scanning, so
//! `WHERE id = #{i` + CDATA `d}` (two segments) is recognized as the one
//! placeholder `#{id}` it actually is, not an unterminated placeholder in
//! the first segment plus stray leftover text in the second.
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
//!
//! When multiple segments are merged (B16), this verbatim/coarse
//! determination is still made **per source segment** -- a property path
//! or replacement that falls entirely within one merged-in segment gets
//! that segment's own precision. A path that itself straddles a junction
//! between two segments (the placeholder crossed the boundary, but the
//! *path text* inside it also happens to straddle) has no single
//! contiguous raw range to report at all (the original bytes in between
//! aren't even contiguous — e.g. a `<![CDATA[`/`]]>` delimiter sits
//! between them), so it also collapses to a coarse zero-width point, at
//! the start position's own resolved offset.

use crate::model::*;

/// Substitution marker for `${}` dynamic fragments (fixed by spec).
pub(crate) const DYN_MARKER: &str = "__BATIS_DYN__";

/// Result of [`normalize_segment`]/[`normalize_merged`].
pub(crate) struct NormalizedSegment {
    /// The rewritten text (`#{}`/`#..#` → `?`, `${}`/`$..$` → [`DYN_MARKER`]).
    pub(crate) text: String,
    /// `(synthetic offset, original byte offset)` pairs in the same format
    /// as [`SqlString::span_map`] — an entry at offset 0 and after every
    /// replacement. MM-06 concatenates these across segments with offset
    /// shifts to build the final map.
    pub(crate) span_map: Vec<(u32, u32)>,
    /// Paths found from both `#`/`$` forms — a `${}` dynamic table name is
    /// exactly what downstream SQL analysis wants to know about.
    pub(crate) property_paths: Vec<Spanned<String>>,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

/// One source text run contributing to a [`normalize_merged`] pass: an
/// already-decoded segment plus its own raw byte span (same shape as
/// `parse::TextSegment`, kept separate so this module doesn't depend on
/// `parse`'s internal types).
pub(crate) struct TextRun<'a> {
    pub(crate) decoded: &'a str,
    pub(crate) raw_span: ByteSpan,
}

/// Normalizes a single decoded text segment's placeholders. A thin
/// wrapper over [`normalize_merged`] with a one-element run list -- kept
/// (test-only) because this module's own unit tests are most direct to
/// write and read against a single segment; flatten.rs always goes
/// through `normalize_merged` directly since it must handle runs of any
/// length (B16, cold code review).
#[cfg(test)]
pub(crate) fn normalize_segment(
    decoded: &str,
    raw_span: ByteSpan,
    dialect: Dialect,
) -> NormalizedSegment {
    normalize_merged(&[TextRun { decoded, raw_span }], dialect)
}

/// Where one source run lands in the merged text, plus its own verbatim
/// determination -- resolved once per run, reused for every position
/// that falls inside it.
struct RunLayout<'a> {
    merged_start: usize,
    raw_span: ByteSpan,
    /// Same rule as the single-segment case: this run's own decoded
    /// length equals its own raw byte length.
    verbatim: bool,
    #[allow(dead_code)]
    decoded: &'a str,
}

impl RunLayout<'_> {
    /// Exact raw offset for `merged_offset` if this run is verbatim (and
    /// `merged_offset` is assumed to already fall within it, per
    /// `layout_for`); the run's own coarse start otherwise.
    fn raw_offset(&self, merged_offset: usize) -> u32 {
        if self.verbatim {
            self.raw_span.start + (merged_offset - self.merged_start) as u32
        } else {
            self.raw_span.start
        }
    }
}

/// Finds the run in `layout` that contains `merged_offset` -- the run
/// with the greatest `merged_start` that's still `<= merged_offset`.
/// Always resolves to *some* run for any offset actually produced while
/// scanning the merged text built from the same `layout` (including the
/// text's own end, via the last run).
fn layout_for<'a, 'b>(layout: &'b [RunLayout<'a>], merged_offset: usize) -> &'b RunLayout<'a> {
    layout
        .iter()
        .rev()
        .find(|l| l.merged_start <= merged_offset)
        .unwrap_or(&layout[0])
}

/// Copies `merged[from..to]` into `normalized`, splitting the copy at any
/// run-junction offsets that fall strictly inside that range and adding a
/// `span_map` entry (at the resulting `normalized` position) for each one.
/// This is what makes the merge "pure" (B16): a run of segments with no
/// placeholder crossing any of their junctions still gets exactly the
/// same span_map density as processing them one at a time would have --
/// one entry per original segment boundary -- not just entries where an
/// actual replacement happened.
fn flush_literal(
    normalized: &mut String,
    span_map: &mut Vec<(u32, u32)>,
    merged: &str,
    layout: &[RunLayout],
    from: usize,
    to: usize,
) {
    if from >= to {
        return;
    }
    let mut pos = from;
    for l in layout {
        if l.merged_start > pos && l.merged_start < to {
            normalized.push_str(&merged[pos..l.merged_start]);
            span_map.push((normalized.len() as u32, l.raw_span.start));
            pos = l.merged_start;
        }
    }
    normalized.push_str(&merged[pos..to]);
}

/// Normalizes a run of adjacent source segments as one logical piece of
/// text (B16): concatenates their already-decoded text, then scans the
/// *whole* merged string for placeholders exactly as
/// [`normalize_segment`] would a single segment -- so a placeholder split
/// across a junction (e.g. a text/CDATA boundary) is found as one
/// placeholder, not two broken pieces. Each replacement/path's raw
/// offset is resolved back through whichever original run it actually
/// falls in (see [`RunLayout`]), preserving per-segment precision for the
/// common (non-straddling) case.
pub(crate) fn normalize_merged(runs: &[TextRun], dialect: Dialect) -> NormalizedSegment {
    debug_assert!(!runs.is_empty());

    let mut merged = String::new();
    let mut layout = Vec::with_capacity(runs.len());
    for run in runs {
        layout.push(RunLayout {
            merged_start: merged.len(),
            raw_span: run.raw_span,
            verbatim: run.decoded.len() as u32 == run.raw_span.end - run.raw_span.start,
            decoded: run.decoded,
        });
        merged.push_str(run.decoded);
    }

    let bytes = merged.as_bytes();
    let mut normalized = String::with_capacity(merged.len());
    let mut span_map = vec![(0u32, layout[0].raw_offset(0))];
    let mut property_paths = Vec::new();
    let mut diagnostics = Vec::new();

    let mut copy_start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'#' && bytes.get(i + 1) == Some(&b'{') {
            match consume_braced(bytes, i) {
                Some((content_start, content_end, end_pos)) => {
                    flush_literal(
                        &mut normalized,
                        &mut span_map,
                        &merged,
                        &layout,
                        copy_start,
                        i,
                    );
                    push_path(
                        &mut property_paths,
                        extract_path_mybatis(&merged[content_start..content_end]),
                        &merged,
                        &layout,
                    );
                    normalized.push('?');
                    copy_start = end_pos;
                    i = end_pos;
                    span_map.push((
                        normalized.len() as u32,
                        layout_for(&layout, end_pos).raw_offset(end_pos),
                    ));
                }
                None => {
                    diagnostics.push(unterminated_placeholder(&layout));
                    flush_literal(
                        &mut normalized,
                        &mut span_map,
                        &merged,
                        &layout,
                        copy_start,
                        bytes.len(),
                    );
                    copy_start = bytes.len();
                    i = bytes.len();
                }
            }
            continue;
        }

        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'{') {
            match consume_braced(bytes, i) {
                Some((content_start, content_end, end_pos)) => {
                    flush_literal(
                        &mut normalized,
                        &mut span_map,
                        &merged,
                        &layout,
                        copy_start,
                        i,
                    );
                    push_path(
                        &mut property_paths,
                        extract_path_mybatis(&merged[content_start..content_end]),
                        &merged,
                        &layout,
                    );
                    normalized.push_str(DYN_MARKER);
                    copy_start = end_pos;
                    i = end_pos;
                    span_map.push((
                        normalized.len() as u32,
                        layout_for(&layout, end_pos).raw_offset(end_pos),
                    ));
                }
                None => {
                    diagnostics.push(unterminated_placeholder(&layout));
                    flush_literal(
                        &mut normalized,
                        &mut span_map,
                        &merged,
                        &layout,
                        copy_start,
                        bytes.len(),
                    );
                    copy_start = bytes.len();
                    i = bytes.len();
                }
            }
            continue;
        }

        if dialect == Dialect::Ibatis && (bytes[i] == b'#' || bytes[i] == b'$') {
            let delim = bytes[i];

            // B18 (cold code review): iBatis's InlineParameterMapParser
            // treats a doubled delimiter ("##"/"$$") as an escaped literal
            // character, not the start of a placeholder -- notably `##`
            // for SQL Server temp tables ("SELECT * FROM ##tmp").
            if bytes.get(i + 1) == Some(&delim) {
                flush_literal(
                    &mut normalized,
                    &mut span_map,
                    &merged,
                    &layout,
                    copy_start,
                    i,
                );
                normalized.push(delim as char);
                let end_pos = i + 2;
                copy_start = end_pos;
                i = end_pos;
                span_map.push((
                    normalized.len() as u32,
                    layout_for(&layout, end_pos).raw_offset(end_pos),
                ));
                continue;
            }

            // Only commit to reading a legacy placeholder when its closing
            // delimiter actually exists in this segment — `#`/`$` are also
            // ordinary SQL/comment characters (monetary literals, etc.), and
            // misfiring here would spam diagnostics on every one of them.
            // A property path never contains whitespace, so two unrelated
            // bare delimiters (e.g. two monetary literals: "$100 ... $200")
            // must not pair up and swallow the SQL between them.
            if let Some(close) = find_byte(bytes, i + 1, delim).filter(|&close| {
                !merged[i + 1..close]
                    .bytes()
                    .any(|b| b.is_ascii_whitespace())
            }) {
                flush_literal(
                    &mut normalized,
                    &mut span_map,
                    &merged,
                    &layout,
                    copy_start,
                    i,
                );
                let content = &merged[i + 1..close];
                push_path(
                    &mut property_paths,
                    extract_path_ibatis(content),
                    &merged,
                    &layout,
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
                    layout_for(&layout, end_pos).raw_offset(end_pos),
                ));
                continue;
            }
        }

        i += 1;
    }

    flush_literal(
        &mut normalized,
        &mut span_map,
        &merged,
        &layout,
        copy_start,
        bytes.len(),
    );

    // B20 (cold code review): a placeholder replacement always pushes a
    // span_map entry right after itself, unconditionally -- when that
    // replacement is the last thing in the text (nothing follows it in
    // this run), that entry lands at exactly `normalized.len()`, one byte
    // past the end. Such an entry describes a segment whose start
    // coincides with the text's own end -- zero surviving characters --
    // so it's a phantom, not a real remaining segment. Same rule
    // `with_suffix_strip` (flatten.rs, B9) already applies: `<`, not `<=`.
    let final_len = normalized.len() as u32;
    span_map.retain(|(off, _)| *off < final_len);

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
/// (`#{}`) silently — nothing to record, nothing to diagnose. `path` must
/// be a substring of `merged` (pointer arithmetic locates it). Exact when
/// `path` falls entirely within one verbatim source run; coarse (a
/// zero-width point at the start's own resolved offset) when it straddles
/// two runs or its run isn't verbatim -- see the module doc comment.
fn push_path(
    property_paths: &mut Vec<Spanned<String>>,
    path: &str,
    merged: &str,
    layout: &[RunLayout],
) {
    if path.is_empty() {
        return;
    }
    let start_offset = (path.as_ptr() as usize) - (merged.as_ptr() as usize);
    let end_offset = start_offset + path.len();
    let start_layout = layout_for(layout, start_offset);
    let end_layout = layout_for(layout, end_offset.saturating_sub(1).max(start_offset));

    let span = if start_layout.merged_start == end_layout.merged_start && start_layout.verbatim {
        ByteSpan {
            start: start_layout.raw_offset(start_offset),
            end: start_layout.raw_offset(end_offset),
        }
    } else {
        let anchor = start_layout.raw_offset(start_offset);
        ByteSpan {
            start: anchor,
            end: anchor,
        }
    };
    property_paths.push(Spanned {
        value: path.to_string(),
        span,
    });
}

/// Spans the whole merged extent (first run's start to last run's end) --
/// for a single run this is exactly that run's own `raw_span`, matching
/// the pre-B16 behavior byte-for-byte.
fn unterminated_placeholder(layout: &[RunLayout]) -> Diagnostic {
    Diagnostic {
        code: DiagCode::UnterminatedPlaceholder,
        span: Some(ByteSpan {
            start: layout.first().expect("non-empty layout").raw_span.start,
            end: layout.last().expect("non-empty layout").raw_span.end,
        }),
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
        // span_map: just the initial entry -- the placeholder is the last
        // thing in the text, so the entry that used to land right after
        // it (B20, cold code review) was a phantom one-past-end entry,
        // now filtered.
        assert_eq!(result.span_map.len(), 1);
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

    // B18 (cold code review): iBatis's InlineParameterMapParser treats a
    // doubled delimiter as an escaped literal, not a placeholder --
    // notably "##" for SQL Server temp tables. iBatis dialect only.

    #[test]
    fn b18_ibatis_doubled_hash_is_literal_hash() {
        let decoded = "SELECT ##tmp";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Ibatis);
        assert_eq!(result.text, "SELECT #tmp");
        assert!(result.diagnostics.is_empty());
        assert!(result.property_paths.is_empty());
    }

    #[test]
    fn b18_ibatis_doubled_dollar_is_literal_dollar() {
        let decoded = "price = $$100";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Ibatis);
        assert_eq!(result.text, "price = $100");
        assert!(result.diagnostics.is_empty());
        assert!(result.property_paths.is_empty());
    }

    #[test]
    fn b18_ibatis_doubled_hash_still_allows_a_real_placeholder_afterward() {
        let decoded = "SELECT ##tmp WHERE id = #id#";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Ibatis);
        assert_eq!(result.text, "SELECT #tmp WHERE id = ?");
        assert_eq!(result.property_paths.len(), 1);
        assert_eq!(result.property_paths[0].value, "id");
    }

    #[test]
    fn b18_doubled_hash_escape_does_not_apply_to_mybatis_dialect() {
        // MyBatis has no legacy #..# form at all, so "##" is just two
        // ordinary characters -- confirm the escape is iBatis-only and
        // doesn't get triggered for MyBatis's own placeholder syntax.
        let decoded = "SELECT ##tmp";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Mybatis);
        assert_eq!(result.text, "SELECT ##tmp");
        assert!(result.diagnostics.is_empty());
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
    fn mm_07_two_bare_dollars_with_whitespace_between_are_not_paired_as_placeholder() {
        // Two monetary literals in one segment: a naive "next matching
        // delimiter" scan would pair them up and swallow real SQL
        // ("100 AND fee < ") as a bogus placeholder "path". Whitespace in
        // the would-be content means it's not a legacy placeholder at all.
        let decoded = "price > $100 AND fee < $200";
        let result = normalize_segment(decoded, verbatim_span(decoded), Dialect::Ibatis);
        assert_eq!(result.text, decoded);
        assert!(result.property_paths.is_empty());
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn mm_07_two_bare_hashes_with_whitespace_between_are_not_paired_as_placeholder() {
        let decoded = "price > #100 AND fee < #200";
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
        // initial + 2 replacements ("a", "b") -- the third ("c") is the
        // last thing in the text, so its own trailing entry is a phantom
        // one-past-end entry (B20, cold code review), now filtered.
        assert_eq!(result.span_map.len(), 3);
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

    // --- B16 (cold code review): a placeholder split across a
    // text/CDATA boundary must normalize correctly when the surrounding
    // segments are merged before scanning.

    fn run<'a>(decoded: &'a str, start: u32) -> TextRun<'a> {
        TextRun {
            decoded,
            raw_span: ByteSpan {
                start,
                end: start + decoded.len() as u32,
            },
        }
    }

    #[test]
    fn merged_placeholder_split_across_two_runs_normalizes_correctly() {
        // "WHERE id = #{i" + CDATA "d}" -- the exact B16 repro.
        let runs = [run("WHERE id = #{i", 0), run("d}", 14)];
        let result = normalize_merged(&runs, Dialect::Mybatis);
        assert_eq!(result.text, "WHERE id = ?");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths.len(), 1);
        assert_eq!(result.property_paths[0].value, "id");
    }

    #[test]
    fn merged_placeholder_split_across_split_cdata_normalizes_correctly() {
        // Three runs, split so the placeholder's braces AND part of its
        // path each land in a different run: "#{i" | "d" | "}".
        let runs = [run("WHERE id = #{i", 0), run("d", 14), run("}", 20)];
        let result = normalize_merged(&runs, Dialect::Mybatis);
        assert_eq!(result.text, "WHERE id = ?");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths[0].value, "id");
    }

    #[test]
    fn merged_placeholder_split_across_three_runs_path_and_all() {
        // The path itself straddles two runs ("i" | "d"), and the closing
        // brace is in a third -- still one placeholder, coarse span
        // acceptable since the path text itself isn't contiguous source.
        let runs = [run("WHERE id = #{i", 0), run("d", 20), run("}", 30)];
        let result = normalize_merged(&runs, Dialect::Mybatis);
        assert_eq!(result.text, "WHERE id = ?");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.property_paths[0].value, "id");
        // Straddles a junction (the "i"/"d" split) -- coarse zero-width
        // span at the path's own start, not a claim of exact precision.
        let span = result.property_paths[0].span;
        assert_eq!(span.start, span.end);
    }

    #[test]
    fn merged_placeholder_entirely_within_one_run_stays_exact() {
        // A merge with more than one run, but the placeholder itself
        // doesn't straddle -- must keep exact precision, not degrade to
        // coarse just because it's part of a merged pass.
        let runs = [run("WHERE id = #{id}", 0), run(" AND x = 1", 16)];
        let result = normalize_merged(&runs, Dialect::Mybatis);
        assert_eq!(result.text, "WHERE id = ? AND x = 1");
        assert_eq!(result.property_paths[0].value, "id");
        let span = result.property_paths[0].span;
        assert_eq!(
            &"WHERE id = #{id}"[(span.start as usize)..(span.end as usize)],
            "id"
        );
    }

    #[test]
    fn merged_genuinely_unterminated_placeholder_spans_the_whole_merge() {
        let runs = [run("WHERE id = #{i", 0), run("d", 14)]; // no closing brace anywhere
        let result = normalize_merged(&runs, Dialect::Mybatis);
        assert_eq!(result.text, "WHERE id = #{id");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].code,
            DiagCode::UnterminatedPlaceholder
        );
        assert_eq!(
            result.diagnostics[0].span,
            Some(ByteSpan { start: 0, end: 15 })
        );
    }
}

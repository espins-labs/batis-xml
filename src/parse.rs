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
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::collections::HashSet;

/// Per-spec cap (10 MB) on input handed to the parser — oversize input is
/// absorbed as `mapper: None` + `OversizeInput` rather than attempting to
/// tokenize an arbitrarily large document.
///
/// `pub(crate)` so `lib.rs` can check it against the *raw byte* length
/// before ever decoding (cold review R2/B5) -- checking only here, after
/// decoding, means a huge input (e.g. 1 GB) still pays for a full decode
/// and allocation before being rejected. Both checks stay: this one is a
/// defense-in-depth backstop for the case a multi-byte legacy encoding
/// expands during decoding (e.g. EUC-KR -> UTF-8 can grow the byte count).
pub(crate) const OVERSIZE_LIMIT: usize = 10 * 1024 * 1024;

/// MM-01: identifies the root element and derives the dialect from its
/// name (`<mapper>` → MyBatis, `<sqlMap>` → iBatis).
///
/// A19 (cold code review): a leading U+FEFF (byte-order mark left in an
/// already-decoded string -- e.g. a caller that read a UTF-8 file without
/// stripping its BOM before calling `parse`) is stripped *here*, before
/// the string is used for anything. Two independent things go wrong if
/// it isn't: quick-xml's `Reader::from_str` silently skips a leading BOM
/// itself and reports `buffer_position()` relative to the *post-BOM*
/// content, while every span this function computes indexes the original
/// (still-BOM-prefixed) string -- a 3-byte skew that corrupts every
/// slice taken through `source[span.start..span.end]` for the rest of
/// the document. Stripping it ourselves first means quick-xml's
/// positions and this function's indexing agree, and it makes this
/// entry point's span semantics match `parse_bytes`' documented contract
/// (`ParseResult::encoding`'s doc comment): every span is relative to
/// the BOM-stripped content, never the original (possibly BOM-prefixed)
/// input.
///
/// A21 (cold code review): a *single* `strip_prefix` only removes one
/// BOM. quick-xml's own BOM skip is also one-shot (verified directly
/// against its reader: a second leading U+FEFF comes back as a leftover
/// `Text` event, not silently consumed), so with two or more leading
/// BOMs, one `strip_prefix` call left the second BOM in `source` while
/// quick-xml's reader (fed that same once-stripped string) skipped *its*
/// leading BOM too -- the same 3-byte skew A19 fixed for one BOM,
/// reappearing for the second. Stripping in a loop until none remain
/// keeps this function's indexing and quick-xml's positions agreeing
/// regardless of how many redundant BOMs a caller's input carries.
pub(crate) fn parse_str(source: &str) -> ParseResult {
    let mut source = source;
    while let Some(rest) = source.strip_prefix('\u{FEFF}') {
        source = rest;
    }
    if source.len() > OVERSIZE_LIMIT {
        return ParseResult {
            dialect: Dialect::Unknown,
            mapper: None,
            // A15 (cold code review): parse_str's input is always an
            // already-decoded &str, so it's always UTF-8 by Rust's own
            // type guarantee -- regardless of which branch returns here.
            // parse_bytes overrides this with the actual encoding the
            // detection chain used before source ever became a &str.
            encoding: Some("UTF-8".to_string()),
            diagnostics: vec![Diagnostic {
                code: DiagCode::OversizeInput,
                span: None,
                message: format!(
                    "input is {} bytes, over the {OVERSIZE_LIMIT}-byte cap",
                    source.len()
                ),
            }],
        };
    }

    let mut reader = Reader::from_str(source);
    // A18 (cold code review): quick-xml 0.41 defaults `allow_dangling_amp`
    // to `false`, so a bare `&` (or an unterminated reference like `&amp`
    // without a `;`) makes `read_event` return an error that swallows
    // everything up to the next `<` -- silently dropping SQL text and any
    // placeholder inside it. Enabling this turns a dangling `&` into
    // ordinary (if unusual) `Text`, which `capture_body`'s `Event::Text`
    // arm below now specifically diagnoses as `InvalidEntity` rather than
    // losing the content. Every `Reader` this crate constructs needs the
    // same setting -- see also `detect_dialect_str` and `capture_subtree`.
    reader.config_mut().allow_dangling_amp = true;
    let mut diagnostics = Vec::new();
    let mut last_err_pos = None;

    loop {
        let start = reader.buffer_position();
        match reader.read_event() {
            Ok(Event::Start(tag)) => {
                let end = reader.buffer_position();
                let name = tag.local_name();
                let dialect = match name.as_ref() {
                    b"mapper" => Some(Dialect::Mybatis),
                    b"sqlMap" => Some(Dialect::Ibatis),
                    _ => None,
                };
                return match dialect {
                    Some(dialect) => {
                        let (mapper, mut mapper_diags) = build_mapper(
                            source,
                            &mut reader,
                            start as usize,
                            end as usize,
                            dialect,
                        );
                        diagnostics.append(&mut mapper_diags);
                        // B47: the root just closed (build_mapper only
                        // returns once it consumed the root's matching
                        // `End`) -- scan whatever follows for the same two
                        // narrow anomalies covered everywhere else in this
                        // tree, instead of returning without ever reading
                        // another event.
                        scan_trailing_content(source, &mut reader, &mut diagnostics);
                        ParseResult {
                            dialect,
                            mapper: Some(mapper),
                            encoding: Some("UTF-8".to_string()),
                            diagnostics,
                        }
                    }
                    None => {
                        diagnostics.push(Diagnostic {
                            code: DiagCode::UnknownElement,
                            span: Some(ByteSpan {
                                start: start as u32,
                                end: end as u32,
                            }),
                            message: format!(
                                "root element <{}> is not a mapper/sqlMap",
                                String::from_utf8_lossy(name.as_ref())
                            ),
                        });
                        ParseResult {
                            dialect: Dialect::Unknown,
                            mapper: None,
                            encoding: Some("UTF-8".to_string()),
                            diagnostics,
                        }
                    }
                };
            }
            Ok(Event::Empty(tag)) => {
                let end = reader.buffer_position();
                let name = tag.local_name();
                let name = name.as_ref();
                return match name {
                    b"mapper" => {
                        let (mapper, mut mapper_diags) =
                            mapper_with_namespace(source, start as usize, end as usize);
                        diagnostics.append(&mut mapper_diags);
                        // B47: a self-closed root is already "complete" the
                        // instant this Empty event was read -- same
                        // trailing scan as the Start/build_mapper case.
                        scan_trailing_content(source, &mut reader, &mut diagnostics);
                        ParseResult {
                            dialect: Dialect::Mybatis,
                            mapper: Some(mapper),
                            encoding: Some("UTF-8".to_string()),
                            diagnostics,
                        }
                    }
                    b"sqlMap" => {
                        let (mapper, mut mapper_diags) =
                            mapper_with_namespace(source, start as usize, end as usize);
                        diagnostics.append(&mut mapper_diags);
                        // B47: see the "mapper" arm above.
                        scan_trailing_content(source, &mut reader, &mut diagnostics);
                        ParseResult {
                            dialect: Dialect::Ibatis,
                            mapper: Some(mapper),
                            encoding: Some("UTF-8".to_string()),
                            diagnostics,
                        }
                    }
                    other => {
                        diagnostics.push(Diagnostic {
                            code: DiagCode::UnknownElement,
                            span: Some(ByteSpan {
                                start: start as u32,
                                end: end as u32,
                            }),
                            message: format!(
                                "root element <{}> is not a mapper/sqlMap",
                                String::from_utf8_lossy(other)
                            ),
                        });
                        ParseResult {
                            dialect: Dialect::Unknown,
                            mapper: None,
                            encoding: Some("UTF-8".to_string()),
                            diagnostics,
                        }
                    }
                };
            }
            Ok(Event::Eof) => {
                diagnostics.push(Diagnostic {
                    code: DiagCode::UnknownElement,
                    span: None,
                    message: "no root element found".to_string(),
                });
                return ParseResult {
                    dialect: Dialect::Unknown,
                    mapper: None,
                    encoding: Some("UTF-8".to_string()),
                    diagnostics,
                };
            }
            Err(err) => {
                let pos = reader.error_position();
                // Recovery rules 2/4: quick-xml's tokenizer has already
                // advanced past the offending bytes by the time it returns
                // an error (verified: the next read_event() picks up at
                // the next recognizable token) — so recovering is just "do
                // not treat this as fatal", not manual byte-scanning for
                // the next `<`. Only bail if we're stuck at the same
                // position (defends the "no infinite loop" invariant
                // against any future quick-xml edge case).
                if last_err_pos == Some(pos) {
                    diagnostics.push(unclosed_tag(
                        pos as usize,
                        pos as usize,
                        format!("XML parse error (unrecoverable): {err}"),
                    ));
                    return ParseResult {
                        dialect: Dialect::Unknown,
                        mapper: None,
                        encoding: Some("UTF-8".to_string()),
                        diagnostics,
                    };
                }
                last_err_pos = Some(pos);
                diagnostics.push(recovery_diagnostic(&err, pos));
            }
            // B44 (cold code review, major): a bare `&` before the root
            // element (e.g. `&<mapper ...>`) used to fall into the
            // catch-all arm below and vanish with zero diagnostics -- see
            // `dangling_amp_diagnostic`'s doc comment.
            Ok(Event::Text(text)) => {
                let decoded = text
                    .decode()
                    .map(|d| d.into_owned())
                    .unwrap_or_else(|_| source[start as usize..].to_string());
                if let Some(diag) = dangling_amp_diagnostic(&decoded, start as usize) {
                    diagnostics.push(diag);
                }
            }
            // B46 (cold code review, moderate): a named/numeric entity
            // reference before the root element (e.g. `&nbsp;<mapper...>`)
            // used to fall into the catch-all arm below and vanish with
            // zero diagnostics even though the identical reference inside
            // a statement body gets `InvalidEntity` from capture_body's
            // own `GeneralRef` arm -- see `resolve_general_ref`'s doc
            // comment for the shared resolvable/unresolvable semantics.
            // The decoded text itself is discarded either way (pre-root
            // content has nowhere to go), same as ordinary pre-root text.
            Ok(Event::GeneralRef(_entity_ref)) => {
                let end = reader.buffer_position() as usize;
                let raw_span = ByteSpan {
                    start: start as u32,
                    end: end as u32,
                };
                let raw_text = &source[raw_span.start as usize..raw_span.end as usize];
                let (_decoded, diag) = resolve_general_ref(raw_text, raw_span);
                if let Some(diag) = diag {
                    diagnostics.push(diag);
                }
            }
            _ => continue,
        }
    }
}

/// B47 (cold code review, minor): `parse_str` used to return as soon as
/// the root element (however it completed -- matching `End`, or a
/// self-closed `Empty`) was accounted for, never reading another event --
/// so `</mapper>&` or `</mapper> &nbsp;` were silently accepted with zero
/// diagnostics, and B44's "regardless of which layer" doc comment
/// overstated coverage (it never covered *after* the root).
///
/// Continues reading events (on the same `reader`, which is already
/// positioned right after the root closed) until `Eof`, applying only the
/// two diagnostics B44/B46 established for mapper-level/pre-root content:
/// a dangling `&` (`Event::Text`) and an unresolvable entity reference
/// (`Event::GeneralRef`). Deliberately narrow scope, matching this
/// function's name: any *other* trailing content (stray elements, stray
/// well-formed text, comments, a second root-like element) stays silently
/// ignored, exactly as it was before this fix -- B47 only closes the
/// dangling-`&`/unresolvable-entity gap, it does not attempt full
/// validation of trailing garbage.
fn scan_trailing_content(
    source: &str,
    reader: &mut Reader<&[u8]>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut last_err_pos = None;
    loop {
        let start = reader.buffer_position();
        match reader.read_event() {
            Ok(Event::Eof) => return,
            Ok(Event::Text(text)) => {
                let decoded = text
                    .decode()
                    .map(|d| d.into_owned())
                    .unwrap_or_else(|_| source[start as usize..].to_string());
                if let Some(diag) = dangling_amp_diagnostic(&decoded, start as usize) {
                    diagnostics.push(diag);
                }
            }
            Ok(Event::GeneralRef(_entity_ref)) => {
                let end = reader.buffer_position() as usize;
                let raw_span = ByteSpan {
                    start: start as u32,
                    end: end as u32,
                };
                let raw_text = &source[raw_span.start as usize..raw_span.end as usize];
                let (_decoded, diag) = resolve_general_ref(raw_text, raw_span);
                if let Some(diag) = diag {
                    diagnostics.push(diag);
                }
            }
            Err(err) => {
                // Same stuck-guard as every other loop in this module
                // (recovery rules 2/4): quick-xml has already
                // resynchronized past a recoverable error by the time it
                // returns one, so only bail out if genuinely stuck at the
                // same position -- otherwise trailing malformed markup
                // after the root could spin forever.
                let pos = reader.error_position();
                if last_err_pos == Some(pos) {
                    return;
                }
                last_err_pos = Some(pos);
                let _ = err;
            }
            // Everything else after the root -- stray elements, ordinary
            // text, comments, another root-like element -- stays out of
            // scope per this function's doc comment.
            _ => continue,
        }
    }
}

/// Cheap dialect pre-check (MM-01 logic only): scans for the root
/// element's name and returns the corresponding dialect, without capturing
/// statement bodies, fragments, resultMaps, or flattening dynamic SQL.
/// Applies the same oversize gate as `parse_str` for exact parity with
/// `parse_str`'s dialect on oversize input -- see `detect_dialect`'s
/// contract test (must agree with the full parse's dialect across the
/// entire conformance corpus).
pub(crate) fn detect_dialect_str(source: &str) -> Dialect {
    if source.len() > OVERSIZE_LIMIT {
        return Dialect::Unknown;
    }

    let mut reader = Reader::from_str(source);
    // A18: see parse_str's identical setting -- this reader independently
    // needs it too, since a dangling `&` before the root element would
    // otherwise error out this cheap pre-check the same way.
    reader.config_mut().allow_dangling_amp = true;
    let mut last_err_pos = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag)) | Ok(Event::Empty(tag)) => {
                return match tag.local_name().as_ref() {
                    b"mapper" => Dialect::Mybatis,
                    b"sqlMap" => Dialect::Ibatis,
                    _ => Dialect::Unknown,
                };
            }
            Ok(Event::Eof) => return Dialect::Unknown,
            Err(err) => {
                let pos = reader.error_position();
                // Same stuck-guard as parse_str's Err arm: quick-xml has
                // already resynchronized past recoverable errors by the
                // time it returns one, so only bail if genuinely stuck.
                if last_err_pos == Some(pos) {
                    return Dialect::Unknown;
                }
                last_err_pos = Some(pos);
                let _ = err;
            }
            _ => continue,
        }
    }
}

/// MM-02: extracts the `namespace` attribute from the root tag's raw byte
/// range `[tag_start, tag_end)`. Missing attribute (iBatis no-namespace
/// mode) yields `None`; no synthesis. A duplicate `namespace` attribute
/// keeps the first value (recovery rule 3) and reports the rest.
fn mapper_with_namespace(
    source: &str,
    tag_start: usize,
    tag_end: usize,
) -> (Mapper, Vec<Diagnostic>) {
    let attrs = scan_attributes(source.as_bytes(), tag_start, tag_end);
    let (namespace, diagnostics) = attr_value_spanned(source, &attrs, b"namespace");
    let mapper = Mapper {
        namespace,
        statements: Vec::new(),
        fragments: Vec::new(),
        result_maps: Vec::new(),
    };
    (mapper, diagnostics)
}

/// MM-03: builds the mapper, including its direct-child statement elements
/// (`select`/`insert`/`update`/`delete` + iBatis `procedure`/`statement`).
/// `reader` has just produced the root's `Start` event (`root_start`,
/// `root_tag_end` bound that event's raw bytes); this walks siblings until
/// the root's matching `End`.
fn build_mapper(
    source: &str,
    reader: &mut Reader<&[u8]>,
    root_start: usize,
    root_tag_end: usize,
    dialect: Dialect,
) -> (Mapper, Vec<Diagnostic>) {
    let root_attrs = scan_attributes(source.as_bytes(), root_start, root_tag_end);
    let (namespace, mut diagnostics) = attr_value_spanned(source, &root_attrs, b"namespace");

    let mut statements = Vec::new();
    let mut fragments = Vec::new();
    let mut result_maps = Vec::new();
    let mut seen_ids: HashSet<(String, Option<String>)> = HashSet::new();
    let mut seen_fragment_ids: HashSet<String> = HashSet::new();
    let mut seen_result_map_ids: HashSet<String> = HashSet::new();
    let mut last_err_pos = None;

    loop {
        let child_start = reader.buffer_position();
        match reader.read_event() {
            Ok(Event::End(_)) => break, // root closed
            Ok(Event::Eof) => {
                diagnostics.push(unclosed_tag(
                    root_start,
                    source.len(),
                    "root element was never closed",
                ));
                break;
            }
            Err(err) => {
                let pos = reader.error_position();
                // See parse_str's Err arm: quick-xml already resynchronized
                // internally by the time it returns the error (recovery
                // rules 2/4) — only bail if genuinely stuck.
                if last_err_pos == Some(pos) {
                    diagnostics.push(unclosed_tag(
                        pos as usize,
                        source.len(),
                        format!("XML parse error (unrecoverable): {err}"),
                    ));
                    break;
                }
                last_err_pos = Some(pos);
                diagnostics.push(recovery_diagnostic(&err, pos));
                continue;
            }
            Ok(Event::Start(tag)) => {
                let tag_end = reader.buffer_position();
                let local_name = tag.local_name();
                let local_name = local_name.as_ref();

                if let Some(kind) = statement_kind(local_name) {
                    let (mut statement, mut diags) = build_statement(
                        source,
                        kind,
                        child_start as usize,
                        tag_end as usize,
                        &mut seen_ids,
                    );
                    diagnostics.append(&mut diags);

                    // MM-08: walks the body for span-preserving text/CDATA
                    // segments (also finds the matching End, like
                    // skip_subtree). Segments feed MM-06/07 assembly later;
                    // Statement.sql stays a placeholder until then.
                    let (segments, mut body_diags, truncated) =
                        capture_body(source, reader, child_start as usize);
                    diagnostics.append(&mut body_diags);
                    // Non-empty element: extend the header-only span (set
                    // in build_statement) to the true subtree end.
                    statement.span.end = reader.buffer_position() as u32;

                    // A6: pull any top-level <selectKey> children out
                    // before the parent's own MM-05/MM-06 passes ever see
                    // them -- otherwise its body text gets concatenated
                    // straight into the parent SQL (see
                    // extract_select_keys's doc comment).
                    let (select_key_statements, segments) = extract_select_keys(
                        source,
                        dialect,
                        statement.id.as_ref(),
                        statement.database_id.as_ref(),
                        segments,
                        &mut diagnostics,
                        &mut seen_ids,
                    );

                    // MM-05: lift top-level <include> markers.
                    let (includes, mut include_diags) = lift_includes(source, dialect, &segments);
                    diagnostics.append(&mut include_diags);

                    // MM-06/07: flatten dynamic tags into SqlText and
                    // normalize placeholders per text segment (including
                    // ones nested inside <if>/<choose>/etc.) to populate
                    // property_paths. flatten_body also finds <include>
                    // markers nested inside dynamic tags (invisible to
                    // lift_includes above, which only sees top-level
                    // children) — merge_includes dedupes the overlap
                    // between the two passes by span.
                    let mut flattened = crate::flatten::flatten_body(source, dialect, &segments);
                    diagnostics.append(&mut flattened.diagnostics);
                    statement.includes = merge_includes(includes, flattened.found_includes);
                    statement.sql = flattened.sql;
                    statement.property_paths = flattened.property_paths;

                    statements.push(statement);
                    // A6: the selectKey child(ren) are appended right
                    // after their parent -- a separate MappedStatement,
                    // not part of the parent's SQL.
                    statements.extend(select_key_statements);
                    if truncated {
                        break;
                    }
                    continue;
                }

                if local_name == b"sql" {
                    let (fragment, mut diags) = build_fragment(
                        source,
                        child_start as usize,
                        tag_end as usize,
                        &mut seen_fragment_ids,
                    );
                    diagnostics.append(&mut diags);

                    // Same body walk as statements — MM-04's "nested
                    // include" edge case just means this must not choke on
                    // <include> tags; MM-05 lifts them below.
                    let (segments, mut body_diags, truncated) =
                        capture_body(source, reader, child_start as usize);
                    diagnostics.append(&mut body_diags);
                    let subtree_end = reader.buffer_position() as u32;
                    // B27: a top-level <selectKey> inside a <sql> fragment
                    // has nowhere valid to go (see reject_select_key_in_fragment).
                    let segments = reject_select_key_in_fragment(segments, &mut diagnostics);

                    if let Some(mut fragment) = fragment {
                        fragment.span.end = subtree_end;
                        let (includes, mut include_diags) =
                            lift_includes(source, dialect, &segments);
                        diagnostics.append(&mut include_diags);

                        // MM-06/07: same flattening as statements.
                        // SqlFragment has no property_paths field (fragment
                        // paths are only meaningful once MM-06 inlines a
                        // fragment into a statement), so that part of the
                        // result is discarded — but diagnostics (e.g. an
                        // unterminated placeholder inside the fragment) and
                        // the flattened sql text are kept.
                        let mut flattened =
                            crate::flatten::flatten_body(source, dialect, &segments);
                        diagnostics.append(&mut flattened.diagnostics);
                        fragment.includes = merge_includes(includes, flattened.found_includes);
                        fragment.sql = flattened.sql;

                        fragments.push(fragment);
                    }
                    if truncated {
                        break;
                    }
                    continue;
                }

                // MM-11: iBatis <parameterMap> has no corresponding model
                // field (the public model is final) — it falls through to
                // the generic skip_subtree below like any other
                // unrecognized element. Documented limitation, not a bug:
                // consumers that need parameterMap data aren't served by
                // this crate today.
                if local_name == b"resultMap" {
                    let (result_map, mut diags) = build_result_map(
                        source,
                        child_start as usize,
                        tag_end as usize,
                        &mut seen_result_map_ids,
                    );
                    diagnostics.append(&mut diags);

                    let (segments, mut body_diags, truncated) =
                        capture_body(source, reader, child_start as usize);
                    diagnostics.append(&mut body_diags);
                    let subtree_end = reader.buffer_position() as u32;

                    if let Some(mut result_map) = result_map {
                        result_map.span.end = subtree_end;
                        collect_mappings(
                            source,
                            &segments,
                            &mut result_map.mappings,
                            &mut diagnostics,
                            0,
                        );
                        result_maps.push(result_map);
                    }
                    if truncated {
                        break;
                    }
                    continue;
                }

                match skip_subtree(reader, &mut diagnostics) {
                    SkipOutcome::Eof => {
                        diagnostics.push(unclosed_tag(
                            child_start as usize,
                            source.len(),
                            "element was never closed",
                        ));
                        break;
                    }
                    SkipOutcome::Err(err) => {
                        diagnostics.push(unclosed_tag(
                            child_start as usize,
                            source.len(),
                            format!("XML parse error while skipping element: {err}"),
                        ));
                        break;
                    }
                    SkipOutcome::Closed => {
                        // A14 (cold code review, major): every unrecognized
                        // statement-level element used to vanish here
                        // identically, whether deliberately out of scope
                        // (<cache>) or a genuine typo (<slect> for
                        // <select>) -- the latter silently drops the whole
                        // statement with zero diagnostics. Flag anything
                        // not in the known-ignorable list.
                        let name_str = String::from_utf8_lossy(local_name);
                        if !is_known_ignorable_element(&name_str) {
                            diagnostics.push(Diagnostic {
                                code: DiagCode::UnknownElement,
                                span: Some(ByteSpan {
                                    start: child_start as u32,
                                    end: reader.buffer_position() as u32,
                                }),
                                message: format!(
                                    "unrecognized statement-level element <{name_str}> -- \
                                     not a known statement/fragment/resultMap tag, and not \
                                     one of this crate's known-ignorable elements; ignored \
                                     (possible typo?)"
                                ),
                            });
                        }
                    }
                }
            }
            Ok(Event::Empty(tag)) => {
                let tag_end = reader.buffer_position();
                let local_name = tag.local_name();
                let local_name = local_name.as_ref();

                if let Some(kind) = statement_kind(local_name) {
                    let (statement, mut diags) = build_statement(
                        source,
                        kind,
                        child_start as usize,
                        tag_end as usize,
                        &mut seen_ids,
                    );
                    diagnostics.append(&mut diags);
                    statements.push(statement);
                } else if local_name == b"sql" {
                    let (fragment, mut diags) = build_fragment(
                        source,
                        child_start as usize,
                        tag_end as usize,
                        &mut seen_fragment_ids,
                    );
                    diagnostics.append(&mut diags);
                    if let Some(fragment) = fragment {
                        fragments.push(fragment);
                    }
                } else if local_name == b"resultMap" {
                    let (result_map, mut diags) = build_result_map(
                        source,
                        child_start as usize,
                        tag_end as usize,
                        &mut seen_result_map_ids,
                    );
                    diagnostics.append(&mut diags);
                    if let Some(result_map) = result_map {
                        result_maps.push(result_map);
                    }
                } else {
                    // A14: same unknown-element check as the non-empty
                    // (Event::Start) case above, for a self-closed
                    // unrecognized statement-level element.
                    let name_str = String::from_utf8_lossy(local_name);
                    if !is_known_ignorable_element(&name_str) {
                        diagnostics.push(Diagnostic {
                            code: DiagCode::UnknownElement,
                            span: Some(ByteSpan {
                                start: child_start as u32,
                                end: tag_end as u32,
                            }),
                            message: format!(
                                "unrecognized statement-level element <{name_str}> -- \
                                 not a known statement/fragment/resultMap tag, and not \
                                 one of this crate's known-ignorable elements; ignored \
                                 (possible typo?)"
                            ),
                        });
                    }
                }
            }
            // B44 (cold code review, major): a bare `&` between sibling
            // statements at mapper level (e.g. `<mapper>& stray
            // <select...>`) used to fall into the catch-all arm below
            // and vanish with zero diagnostics -- see
            // `dangling_amp_diagnostic`'s doc comment. A dangling amp
            // *inside* a statement body is still diagnosed exactly once
            // by `capture_body` alone (that Text event is consumed
            // there, never reaches this loop), so this doesn't double up
            // with B36's dedup -- it's a different layer entirely.
            Ok(Event::Text(text)) => {
                let decoded = text
                    .decode()
                    .map(|d| d.into_owned())
                    .unwrap_or_else(|_| source[child_start as usize..].to_string());
                if let Some(diag) = dangling_amp_diagnostic(&decoded, child_start as usize) {
                    diagnostics.push(diag);
                }
            }
            // B46 (cold code review, moderate): same gap as parse_str's
            // pre-root loop, one layer down -- a named/numeric entity
            // reference between sibling statements (e.g. `<mapper>&nbsp;
            // <select...>`) used to vanish with zero diagnostics. See
            // `resolve_general_ref`'s doc comment; the decoded text is
            // discarded either way, same as any other mapper-level text.
            Ok(Event::GeneralRef(_entity_ref)) => {
                let end = reader.buffer_position() as usize;
                let raw_span = ByteSpan {
                    start: child_start as u32,
                    end: end as u32,
                };
                let raw_text = &source[raw_span.start as usize..raw_span.end as usize];
                let (_decoded, diag) = resolve_general_ref(raw_text, raw_span);
                if let Some(diag) = diag {
                    diagnostics.push(diag);
                }
            }
            _ => continue,
        }
    }

    // MM-05: intra-file dangling check. Only Local targets are checked —
    // Qualified (cross-file) and Dynamic (unresolvable) are the consumer's
    // job. Done after the full walk so forward references (a statement
    // above the <sql> it includes) resolve correctly.
    //
    // B22 (cold code review): MyBatis-only. This is a file-local heuristic
    // -- it only ever sees the one mapper file being parsed, so it can't
    // know about `<sql>` fragments defined elsewhere. MyBatis namespaces
    // are per-file and refids are typically local, so a miss is usually a
    // real typo. iBatis fragments are a global cross-file registry by
    // design (any sqlMap can reference any other sqlMap's `<sql>` by short
    // name), so this same heuristic would flag nearly every legitimate
    // cross-file reference as dangling -- almost pure noise. See
    // `DiagCode::DanglingRefid`'s doc comment and README.
    if dialect == Dialect::Mybatis {
        for statement in &statements {
            check_dangling_local_refids(&statement.includes, &seen_fragment_ids, &mut diagnostics);
        }
        for fragment in &fragments {
            check_dangling_local_refids(&fragment.includes, &seen_fragment_ids, &mut diagnostics);
        }
    }

    // NOTE: "unused fragment" detection (spec edge case) needs
    // cross-statement <include> resolution — that's a consumer/linker
    // concern (MM-05 collects refs; nothing here resolves them), not this
    // function's job.
    //
    // B36 (cold code review, minor): a top-level `<include>` is visited by
    // both `lift_includes` (this function, top-level-only) and
    // `flatten_body`'s own descent (`record_include`, which sees every
    // `<include>` including top-level ones) -- see `merge_includes`'s doc
    // comment. `merge_includes` already dedupes the resulting `IncludeRef`
    // entries by span, but each pass calls `attr_value_spanned` on the same
    // attribute independently, so an anomaly *in that attribute scan*
    // (e.g. a duplicated `refid` attribute) was reported once per pass --
    // an identical (code, span) diagnostic twice for one real anomaly.
    // Dedup here, once, across the whole mapper.
    dedup_diagnostics(&mut diagnostics);
    let mapper = Mapper {
        namespace,
        statements,
        fragments,
        result_maps,
    };
    (mapper, diagnostics)
}

/// Removes exact duplicates -- same `code` and same *concrete* `span` --
/// keeping the first occurrence and preserving order. Two diagnostics can
/// share both a code and a real byte span only when they describe the
/// exact same anomaly, reported by more than one independent pass over the
/// same bytes (see the `B36` comment at this function's one call site).
///
/// A `span: None` diagnostic is deliberately left alone: unlike a concrete
/// span, `None` carries no positional information, so two `None`-span
/// diagnostics with the same code are not necessarily the same anomaly --
/// e.g. `BranchLimitExceeded` is `span: None` (whole-statement scope) and
/// legitimately recurs once per statement that independently exceeds the
/// cap; deduping on code alone would silently drop every occurrence past
/// the first real one.
fn dedup_diagnostics(diagnostics: &mut Vec<Diagnostic>) {
    let mut seen: Vec<(DiagCode, ByteSpan)> = Vec::with_capacity(diagnostics.len());
    diagnostics.retain(|d| {
        let Some(span) = d.span else {
            return true;
        };
        let key = (d.code, span);
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
}

/// Maps a statement-like tag's local name to its [`StatementKind`]. `None`
/// means "not a statement" (e.g. `<resultMap>`, `<sql>` — owned by other
/// micro-features).
fn statement_kind(local_name: &[u8]) -> Option<StatementKind> {
    match local_name {
        b"select" => Some(StatementKind::Select),
        b"insert" => Some(StatementKind::Insert),
        b"update" => Some(StatementKind::Update),
        b"delete" => Some(StatementKind::Delete),
        b"procedure" => Some(StatementKind::Procedure),
        b"statement" => Some(StatementKind::Generic),
        _ => None,
    }
}

/// Elements with no corresponding model field that this crate deliberately
/// (not accidentally) skips without a diagnostic, at either statement level
/// (a direct child of `<mapper>`/`<sqlMap>` that isn't a statement/`<sql>`/
/// `<resultMap>`) or dynamic position (nested inside a statement/fragment
/// body, reached via [`crate::flatten::expand_transparent`]'s catch-all).
///
/// Before this list existed, *every*
/// unrecognized element silently vanished the same way, whether it was a
/// deliberately-out-of-scope one like `<cache>` or a genuine typo like
/// `<slect>`/`<iff>` -- there was no way to tell "this crate doesn't model
/// caching" apart from "your mapper XML has a bug". Anything NOT in this
/// list now gets a `DiagCode::UnknownElement` diagnostic instead of
/// silently doing nothing.
///
/// MyBatis: `cache`, `cache-ref`, `parameterMap` (MM-11: no model field).
/// iBatis: `cacheModel`, `typeAlias`, `parameterMap` (same tag name,
/// deliberately out of scope in both dialects).
pub(crate) fn is_known_ignorable_element(local_name: &str) -> bool {
    matches!(
        local_name,
        "cache" | "cache-ref" | "parameterMap" | "cacheModel" | "typeAlias"
    )
}

/// A single variant with empty text and no conditions -- the correct
/// placeholder/final value for an empty body, matching what flattening an
/// *actually empty* segment list produces (`flatten_segments` starts its
/// accumulator at exactly one empty `Alt`). Used for self-closed elements
/// (`<select/>`, `<sql/>`), which never go through `capture_body`/
/// `flatten_body` at all.
///
/// This used to be `SqlText::Variants(Vec::new())`
/// (zero variants) for self-closed elements specifically -- a different,
/// inconsistent shape from `<select></select>` (one empty variant).
/// Consumers matching on `SqlText::Variants(vs)` and indexing `vs[0]`
/// (a reasonable assumption once `Union`'s branch-count is known to be
/// low) would panic on a self-closed statement they had no way to
/// distinguish from an empty-bodied one in the schema.
fn empty_sql_variants() -> SqlText {
    SqlText::Variants(vec![SqlVariant {
        text: SqlString {
            text: String::new(),
            span_map: Vec::new(),
        },
        conditions: Vec::new(),
    }])
}

/// Builds one [`Statement`] from its tag's raw byte range. `seen_ids`
/// tracks `(id, databaseId)` pairs already collected in this mapper so a
/// repeated id — legitimate under MyBatis `databaseId` branching when the
/// `databaseId` differs — only reports `DuplicateStatementId` when both
/// match.
fn build_statement(
    source: &str,
    kind: StatementKind,
    tag_start: usize,
    tag_end: usize,
    seen_ids: &mut HashSet<(String, Option<String>)>,
) -> (Statement, Vec<Diagnostic>) {
    let attrs = scan_attributes(source.as_bytes(), tag_start, tag_end);
    let (id, mut diagnostics) = attr_value_spanned(source, &attrs, b"id");
    let (database_id, mut db_diags) = attr_value_spanned(source, &attrs, b"databaseId");
    diagnostics.append(&mut db_diags);

    match &id {
        Some(id) => {
            let key = (
                id.value.clone(),
                database_id.as_ref().map(|d| d.value.clone()),
            );
            if !seen_ids.insert(key) {
                diagnostics.push(Diagnostic {
                    code: DiagCode::DuplicateStatementId,
                    span: Some(id.span),
                    message: format!("duplicate statement id '{}'", id.value),
                });
            }
        }
        None => diagnostics.push(Diagnostic {
            code: DiagCode::MissingStatementId,
            span: Some(ByteSpan {
                start: tag_start as u32,
                end: tag_end as u32,
            }),
            message: "statement is missing an id attribute".to_string(),
        }),
    }

    // MM-09: parameterType/resultType (MyBatis) or parameterClass/
    // resultClass (iBatis) — checking both names regardless of dialect is
    // simpler than threading dialect through, and harmless since a file
    // only ever uses its own dialect's attribute name.
    let (param_class, mut param_diags) =
        read_class_ref(source, &attrs, &[b"parameterType", b"parameterClass"]);
    diagnostics.append(&mut param_diags);
    let (result_class, mut result_diags) =
        read_class_ref(source, &attrs, &[b"resultType", b"resultClass"]);
    diagnostics.append(&mut result_diags);
    let (result_map_ref, mut rm_diags) = attr_value_spanned(source, &attrs, b"resultMap");
    diagnostics.append(&mut rm_diags);

    let statement = Statement {
        kind,
        // Header-only span for now (self-closed elements never get a body
        // walk to extend it); build_mapper overwrites `.end` to the true
        // subtree end for non-empty elements once capture_body finds it.
        span: ByteSpan {
            start: tag_start as u32,
            end: tag_end as u32,
        },
        id,
        database_id,
        // Placeholder for non-self-closed elements (overwritten by MM-08/
        // MM-06 flattening); the final value for self-closed ones, where
        // it's correct as-is (B10: matches an empty-bodied element's shape).
        sql: empty_sql_variants(),
        includes: Vec::new(),
        param_class,
        result_class,
        result_map_ref,
        property_paths: Vec::new(),
    };
    (statement, diagnostics)
}

/// `<selectKey>` used to be treated as a
/// transparent passthrough dynamic tag (flatten.rs's default arm for any
/// tag name it doesn't recognize) -- its body text got concatenated
/// straight into the parent statement's SQL, e.g. `SELECT NEXT VALUE FOR
/// widget_seq INSERT INTO ...`, a two-statement mash blessed by the old
/// `selectkey_passthrough` fixture. MyBatis instead compiles it into a
/// wholly separate `MappedStatement` named `id + "!selectKey"` and removes
/// the node from the parent's own SQL entirely; iBatis's `<selectKey>`
/// (same tag name, same semantics) gets the same treatment here.
///
/// Extracts every top-level `<selectKey>` child from `segments`, returning
/// the synthesized child [`Statement`]s (kind `Select`, own span, own
/// flattened SQL/includes/property_paths) alongside the remaining segments
/// with those markers removed, so the parent's own MM-05/MM-06 passes
/// never see their body text. A `<selectKey>` nested inside another
/// dynamic tag (not a direct child -- not valid MyBatis/iBatis, but not
/// rejected either) is out of scope here and keeps the previous
/// passthrough behavior, same as any other unrecognized nested tag.
/// `<selectKey>` only makes sense as a
/// direct child of an `<insert>`/`<update>` statement (see
/// [`extract_select_keys`], which splits it into its own synthesized
/// `Statement` there) -- a `<sql>` fragment has no `MappedStatement` to
/// attach a synthesized child to, so one appearing as a *top-level* child
/// of `<sql>` used to just silently fall into the generic
/// transparent-passthrough path (its body mashed straight into the
/// fragment's own text). Strips any such child out entirely (dropped, not
/// folded in -- there's nowhere valid for its contribution to go) and
/// reports exactly why, distinct from A14's generic "unrecognized
/// element" catch-all (which would otherwise still fire on it, since
/// `selectKey` isn't a `<sql>`-body dynamic tag either) -- `selectKey` is
/// a real, recognized tag, just invalid in *this* position, so this is a
/// clearer message than "unrecognized".
fn reject_select_key_in_fragment(
    segments: Vec<BodySegment>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<BodySegment> {
    segments
        .into_iter()
        .filter(|segment| {
            if let BodySegment::DynamicTag { name, span } = segment {
                if name == "selectKey" {
                    diagnostics.push(Diagnostic {
                        code: DiagCode::UnknownElement,
                        span: Some(*span),
                        message: "<selectKey> is not valid inside a <sql> fragment -- it \
                                  only makes sense as a direct child of <insert>/<update>; \
                                  ignored here (invalid placement)"
                            .to_string(),
                    });
                    return false;
                }
            }
            true
        })
        .collect()
}

fn extract_select_keys(
    source: &str,
    dialect: Dialect,
    parent_id: Option<&Spanned<String>>,
    parent_database_id: Option<&Spanned<String>>,
    segments: Vec<BodySegment>,
    diagnostics: &mut Vec<Diagnostic>,
    seen_ids: &mut HashSet<(String, Option<String>)>,
) -> (Vec<Statement>, Vec<BodySegment>) {
    let mut children = Vec::new();
    let mut remaining = Vec::with_capacity(segments.len());

    for segment in segments {
        match segment {
            BodySegment::DynamicTag { name, span } if name == "selectKey" => {
                children.push(build_select_key_statement(
                    source,
                    dialect,
                    parent_id,
                    parent_database_id,
                    span,
                    diagnostics,
                    seen_ids,
                ));
            }
            other => remaining.push(other),
        }
    }

    (children, remaining)
}

/// Builds the synthesized child `Statement` for one `<selectKey>` marker —
/// see [`extract_select_keys`].
fn build_select_key_statement(
    source: &str,
    dialect: Dialect,
    parent_id: Option<&Spanned<String>>,
    parent_database_id: Option<&Spanned<String>>,
    span: ByteSpan,
    diagnostics: &mut Vec<Diagnostic>,
    seen_ids: &mut HashSet<(String, Option<String>)>,
) -> Statement {
    let attrs = scan_attributes(source.as_bytes(), span.start as usize, span.end as usize);
    // MM-09: resultType (MyBatis) / resultClass (iBatis) — same dual-name
    // check build_statement uses for every other statement-like tag.
    let (result_class, mut result_diags) =
        read_class_ref(source, &attrs, &[b"resultType", b"resultClass"]);
    diagnostics.append(&mut result_diags);

    // A13 (cold code review, major): read databaseId like every other
    // real statement -- a <selectKey> can legitimately carry its own
    // databaseId (independent of the parent statement's), and previously
    // hardcoding `None` here meant two selectKeys under different
    // databaseIds looked identical (both `id!selectKey`, both
    // `database_id: None`), silently colliding in `seen_ids` and reporting
    // a spurious duplicate even though MyBatis treats them as distinct,
    // dialect-branched statements.
    //
    // A20 (cold code review, major): A13 stopped there, so the *other*
    // direction of the same bug remained -- the canonical dual-dialect
    // pattern (two `<insert id="ins" databaseId="oracle|mysql">`, each
    // with a plain `<selectKey>` that carries no `databaseId` of its
    // own) synthesized two `ins!selectKey` statements that both read
    // `database_id: None`, so they collided in `seen_ids` as a spurious
    // `DuplicateStatementId` even though the parents are legitimately
    // dialect-branched. A `<selectKey>` with no `databaseId` attribute
    // inherits the parent statement's; an explicit one on the
    // `<selectKey>` itself still wins (checked first, so this only
    // fills in the absence).
    let (database_id, mut db_diags) = attr_value_spanned(source, &attrs, b"databaseId");
    diagnostics.append(&mut db_diags);
    let database_id = database_id.or_else(|| parent_database_id.cloned());

    // Synthesized, never read from an `id` attribute (selectKey doesn't
    // have one) -- no MissingStatementId diagnostic either way: a missing
    // parent id is already covered by the parent statement's own
    // diagnostic, and this id is derived, not user-authored.
    let id = parent_id.map(|p| Spanned {
        value: format!("{}!selectKey", p.value),
        span,
    });

    // A13: register the synthesized id (same (id, databaseId) key
    // build_statement uses) in the shared seen_ids set -- previously
    // selectKey's synthesized id was never recorded at all, so neither
    // two selectKeys colliding on the same databaseId, nor a real
    // statement literally named `x!selectKey`, would ever be flagged as
    // DuplicateStatementId, even though both are genuine id collisions in
    // the same output document.
    if let Some(id) = &id {
        let key = (
            id.value.clone(),
            database_id.as_ref().map(|d| d.value.clone()),
        );
        if !seen_ids.insert(key) {
            diagnostics.push(Diagnostic {
                code: DiagCode::DuplicateStatementId,
                span: Some(id.span),
                message: format!("duplicate statement id '{}'", id.value),
            });
        }
    }

    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    diagnostics.append(&mut inner_diags);
    let (includes, mut include_diags) = lift_includes(source, dialect, &inner_segments);
    diagnostics.append(&mut include_diags);
    let mut flattened = crate::flatten::flatten_body(source, dialect, &inner_segments);
    diagnostics.append(&mut flattened.diagnostics);

    Statement {
        kind: StatementKind::Select,
        span,
        id,
        database_id,
        sql: flattened.sql,
        includes: merge_includes(includes, flattened.found_includes),
        param_class: None,
        result_class,
        result_map_ref: None,
        property_paths: flattened.property_paths,
    }
}

/// MM-09: reads the first present attribute among `names` as a
/// [`ClassRef`] (raw, unparsed — alias resolution, generics, arrays are
/// all the consumer's job). Checks every name (not just up to the first
/// match) so duplicate-attribute diagnostics on a later name aren't lost.
fn read_class_ref(
    source: &str,
    attrs: &[RawAttr],
    names: &[&[u8]],
) -> (Option<Spanned<ClassRef>>, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    let mut result = None;
    for name in names {
        let (value, mut diags) = attr_value_spanned(source, attrs, name);
        diagnostics.append(&mut diags);
        if result.is_none() {
            result = value.map(|v| Spanned {
                value: ClassRef { raw: v.value },
                span: v.span,
            });
        }
    }
    (result, diagnostics)
}

/// MM-04: builds one [`SqlFragment`] from a `<sql>` tag's raw byte range.
/// `seen_fragment_ids` is a *separate* id space from statement ids (a
/// fragment and a statement may legitimately share an id without either
/// being a duplicate).
///
/// `SqlFragment.id` is non-optional in the model, so a `<sql>` without an
/// `id` attribute can't be represented — it's dropped (`None` return) with
/// a `MissingStatementId`-coded diagnostic.
fn build_fragment(
    source: &str,
    tag_start: usize,
    tag_end: usize,
    seen_fragment_ids: &mut HashSet<String>,
) -> (Option<SqlFragment>, Vec<Diagnostic>) {
    let attrs = scan_attributes(source.as_bytes(), tag_start, tag_end);
    let (id, mut diagnostics) = attr_value_spanned(source, &attrs, b"id");

    let fragment = match id {
        Some(id) => {
            if !seen_fragment_ids.insert(id.value.clone()) {
                diagnostics.push(Diagnostic {
                    code: DiagCode::DuplicateStatementId,
                    span: Some(id.span),
                    message: format!("duplicate <sql> fragment id '{}'", id.value),
                });
            }
            Some(SqlFragment {
                // See build_statement's span comment — overwritten to the
                // true subtree end for non-empty elements.
                span: ByteSpan {
                    start: tag_start as u32,
                    end: tag_end as u32,
                },
                id,
                // Placeholder/final value, same as Statement.sql (B10) --
                // real text lands in MM-07/MM-06 for non-self-closed <sql>.
                sql: empty_sql_variants(),
                // MM-05's job to populate.
                includes: Vec::new(),
            })
        }
        None => {
            diagnostics.push(Diagnostic {
                code: DiagCode::MissingStatementId,
                span: Some(ByteSpan {
                    start: tag_start as u32,
                    end: tag_end as u32,
                }),
                message: "<sql> fragment is missing an id attribute".to_string(),
            });
            None
        }
    };

    (fragment, diagnostics)
}

/// MM-10: builds one [`ResultMap`] from a `<resultMap>` tag's raw byte
/// range. `seen_result_map_ids` is its own id space, separate from both
/// statement and fragment ids. `mappings` starts empty — the caller fills
/// it in via [`collect_mappings`] once the body has been walked.
///
/// `ResultMap.id` is non-optional in the model, so a `<resultMap>` without
/// an `id` attribute can't be represented — dropped (`None` return) with a
/// `MissingStatementId`-coded diagnostic, same pattern as `<sql>`.
fn build_result_map(
    source: &str,
    tag_start: usize,
    tag_end: usize,
    seen_result_map_ids: &mut HashSet<String>,
) -> (Option<ResultMap>, Vec<Diagnostic>) {
    let attrs = scan_attributes(source.as_bytes(), tag_start, tag_end);
    let (id, mut diagnostics) = attr_value_spanned(source, &attrs, b"id");
    // MM-11: iBatis <resultMap> uses class=, MyBatis uses type=.
    let (type_ref, mut type_diags) = read_class_ref(source, &attrs, &[b"type", b"class"]);
    diagnostics.append(&mut type_diags);
    let (extends, mut extends_diags) = attr_value_spanned(source, &attrs, b"extends");
    diagnostics.append(&mut extends_diags);

    let result_map = match id {
        Some(id) => {
            if !seen_result_map_ids.insert(id.value.clone()) {
                diagnostics.push(Diagnostic {
                    code: DiagCode::DuplicateStatementId,
                    span: Some(id.span),
                    message: format!("duplicate <resultMap> id '{}'", id.value),
                });
            }
            Some(ResultMap {
                // See build_statement's span comment — overwritten to the
                // true subtree end for non-empty elements.
                span: ByteSpan {
                    start: tag_start as u32,
                    end: tag_end as u32,
                },
                id,
                type_ref,
                extends,
                mappings: Vec::new(),
            })
        }
        None => {
            diagnostics.push(Diagnostic {
                code: DiagCode::MissingStatementId,
                span: Some(ByteSpan {
                    start: tag_start as u32,
                    end: tag_end as u32,
                }),
                message: "<resultMap> is missing an id attribute".to_string(),
            });
            None
        }
    };

    (result_map, diagnostics)
}

/// MM-10: walks a `<resultMap>` body's [`BodySegment`]s, appending a
/// [`ColumnMapping`] for each `<id>`/`<result>` child (column/property both
/// optional), and recursing into `<association>`/`<collection>` bodies and
/// `<discriminator>`'s `<case>` children — their mappings flatten into the
/// same `Vec`, matching the model (`ColumnMapping` has no nested-structure
/// field).
///
/// Depth-limited (see [`crate::flatten::DEPTH_LIMIT`]): `depth` is the
/// nesting level of this call (0 at the top), incremented on every
/// `association`/`collection`/`discriminator`-`case` recursion. Pathologically
/// deep nesting would otherwise reach that deep in the Rust call stack
/// before there's any other way to detect it -- a stack overflow aborts
/// the process (uncatchable). At the cap, the remaining subtree is
/// skipped (no further mappings collected) with a diagnostic.
fn collect_mappings(
    source: &str,
    segments: &[BodySegment],
    mappings: &mut Vec<ColumnMapping>,
    diagnostics: &mut Vec<Diagnostic>,
    depth: u32,
) {
    if depth >= crate::flatten::DEPTH_LIMIT {
        diagnostics.push(Diagnostic {
            code: DiagCode::NestingLimitExceeded,
            span: None,
            message: format!(
                "resultMap association/collection/discriminator nesting exceeds the depth cap of {}; remaining subtree skipped",
                crate::flatten::DEPTH_LIMIT
            ),
        });
        return;
    }

    for segment in segments {
        let BodySegment::DynamicTag { name, span } = segment else {
            continue; // whitespace between child elements
        };

        match name.as_str() {
            "id" | "result" => {
                let attrs =
                    scan_attributes(source.as_bytes(), span.start as usize, span.end as usize);
                let (column, mut d) = attr_value_spanned(source, &attrs, b"column");
                diagnostics.append(&mut d);
                let (property, mut d) = attr_value_spanned(source, &attrs, b"property");
                diagnostics.append(&mut d);
                mappings.push(ColumnMapping {
                    column: column.map(|c| c.value),
                    property: property.map(|p| p.value),
                });
            }
            "association" | "collection" => {
                let (inner, mut d, _truncated) = capture_subtree(source, *span);
                diagnostics.append(&mut d);
                collect_mappings(source, &inner, mappings, diagnostics, depth + 1);
            }
            "discriminator" => {
                let (inner, mut d, _truncated) = capture_subtree(source, *span);
                diagnostics.append(&mut d);
                for case in &inner {
                    if let BodySegment::DynamicTag {
                        name: case_name,
                        span: case_span,
                    } = case
                    {
                        if case_name == "case" {
                            let (case_inner, mut cd, _t) = capture_subtree(source, *case_span);
                            diagnostics.append(&mut cd);
                            collect_mappings(source, &case_inner, mappings, diagnostics, depth + 1);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// MM-05: lifts `<include>` [`BodySegment::DynamicTag`] markers (already
/// found by `capture_body`) into `Spanned<IncludeRef>`. Reruns
/// `scan_attributes` over the marker's span to read `refid` — safe because
/// the tokenizer stops at the tag's first `>`, so a full-subtree span
/// (which is all a marker carries) is fine to pass.
fn lift_includes(
    source: &str,
    dialect: Dialect,
    segments: &[BodySegment],
) -> (Vec<Spanned<IncludeRef>>, Vec<Diagnostic>) {
    let mut includes = Vec::new();
    let mut diagnostics = Vec::new();

    for segment in segments {
        let BodySegment::DynamicTag { name, span } = segment else {
            continue;
        };
        if name != "include" {
            continue;
        }

        let attrs = scan_attributes(source.as_bytes(), span.start as usize, span.end as usize);
        let (refid, mut diags) = attr_value_spanned(source, &attrs, b"refid");
        diagnostics.append(&mut diags);

        match refid {
            Some(refid) => {
                let target = classify_include(&refid.value, dialect);
                includes.push(Spanned {
                    value: IncludeRef {
                        raw: refid.value,
                        target,
                    },
                    span: *span,
                });
            }
            None => diagnostics.push(Diagnostic {
                code: DiagCode::DanglingRefid,
                span: Some(*span),
                message: "<include> is missing a refid attribute".to_string(),
            }),
        }
    }

    (includes, diagnostics)
}

/// MM-06b: merges the top-level `<include>`s `lift_includes` found with the
/// (possibly overlapping, possibly nested-only) ones `flatten_body`'s
/// descent found, deduping by span and sorting into document order. A
/// top-level `<include>` is visited by both passes; a nested one (inside
/// `<if>`/`<choose>`/etc.) only by the second.
fn merge_includes(
    top_level: Vec<Spanned<IncludeRef>>,
    nested: Vec<Spanned<IncludeRef>>,
) -> Vec<Spanned<IncludeRef>> {
    let mut seen: HashSet<(u32, u32)> = top_level
        .iter()
        .map(|i| (i.span.start, i.span.end))
        .collect();
    let mut merged = top_level;
    for include in nested {
        if seen.insert((include.span.start, include.span.end)) {
            merged.push(include);
        }
    }
    merged.sort_by_key(|i| i.span.start);
    merged
}

/// MM-05: classifies a `refid` value. `${}`-driven dynamic refids are
/// unresolvable regardless of dialect. Otherwise: MyBatis splits on the
/// *last* dot into `ns.id` (namespaces themselves may contain dots);
/// iBatis has no cross-namespace include syntax — a dot there is the
/// normal local-id convention (`WidgetDAO.commonWhere`) and always stays
/// `Local`. `raw` is preserved either way so consumers can re-derive.
pub(crate) fn classify_include(raw: &str, dialect: Dialect) -> IncludeTarget {
    if raw.contains("${") {
        return IncludeTarget::Dynamic;
    }
    if dialect == Dialect::Mybatis {
        if let Some(dot) = raw.rfind('.') {
            return IncludeTarget::Qualified {
                ns: raw[..dot].to_string(),
                id: raw[dot + 1..].to_string(),
            };
        }
    }
    IncludeTarget::Local(raw.to_string())
}

/// MM-05: intra-file dangling check for `Local` include targets only —
/// `Qualified` (cross-file) and `Dynamic` (unresolvable at parse time) are
/// the consumer's job.
fn check_dangling_local_refids(
    includes: &[Spanned<IncludeRef>],
    fragment_ids: &HashSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for include in includes {
        if let IncludeTarget::Local(id) = &include.value.target {
            if !fragment_ids.contains(id) {
                diagnostics.push(Diagnostic {
                    code: DiagCode::DanglingRefid,
                    span: Some(include.span),
                    message: format!(
                        "<include refid=\"{}\"> has no matching <sql> fragment in this file",
                        include.value.raw
                    ),
                });
            }
        }
    }
}

/// Outcome of consuming a subtree after its opening `Start` event.
enum SkipOutcome {
    /// The matching `End` was found.
    Closed,
    /// Input ended before the matching `End` (recovery rule 1: the parent
    /// closing implicitly closes this element — the caller reports it).
    Eof,
    /// A parse error occurred while skipping.
    Err(quick_xml::Error),
}

/// Consumes events until the `End` that matches the `Start` the caller just
/// read (simple depth counting — assumes well-nested input for the
/// *matching*, but tolerates recoverable reader errors along the way:
/// recovery rules 2/4 mean an orphan/mismatched closing tag or other
/// malformed markup doesn't abort the skip, just gets diagnosed and
/// skipped over). `SkipOutcome::Err` is reserved for the reader getting
/// stuck at the same byte position twice in a row (defends the
/// "no infinite loop" invariant; not expected in practice).
fn skip_subtree(reader: &mut Reader<&[u8]>, diagnostics: &mut Vec<Diagnostic>) -> SkipOutcome {
    let mut depth = 1u32;
    let mut last_err_pos = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    return SkipOutcome::Closed;
                }
            }
            Ok(Event::Eof) => return SkipOutcome::Eof,
            Err(err) => {
                let pos = reader.error_position();
                if last_err_pos == Some(pos) {
                    return SkipOutcome::Err(err);
                }
                last_err_pos = Some(pos);
                diagnostics.push(recovery_diagnostic(&err, pos));
            }
            _ => {}
        }
    }
}

/// Builds an `UnclosedTag` diagnostic spanning `[start, end)`.
fn unclosed_tag(start: usize, end: usize, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        code: DiagCode::UnclosedTag,
        span: Some(ByteSpan {
            start: start as u32,
            end: end as u32,
        }),
        message: message.into(),
    }
}

/// B44 (cold code review, major): A18's dangling-`&` diagnosis lived only
/// in `capture_body` (inside a statement's own body), so a bare `&`
/// encountered at mapper level -- before the root element, or between
/// sibling statements -- fell through those loops' catch-all `_ =>
/// continue` arm and produced *zero* diagnostics (pre-A18, the same input
/// at least produced a recovery diagnostic, since `allow_dangling_amp`
/// wasn't enabled yet). Every anomaly must become a `Diagnostic`
/// (CLAUDE.md rule 4) regardless of which layer of the tree it's in --
/// B44 covered before-root and between-siblings; B47 later closed the
/// remaining gap, *after* the root element closes (`scan_trailing_content`).
///
/// `text_start` is the byte offset the enclosing `Ok(Event::Text(text))`
/// arm's `reader.buffer_position()` was read *before* consuming the
/// event (i.e. where the Text event itself begins) -- quick-xml starts a
/// fresh Text event exactly at a dangling `&` (never mid-run, same
/// property `capture_body`'s A18 comment relies on), so this is also
/// exactly the `&`'s own byte offset when `decoded` starts with one.
///
/// Returns `None` for ordinary text (the overwhelmingly common case: ordinary
/// whitespace/comments between tags), so call sites only pay for a
/// `Vec` push when there's actually an anomaly.
///
/// Span is 1 byte per B43 (`start..start+1`, just the `&`) -- matches
/// `capture_body`'s narrowed span, not the old fat one.
fn dangling_amp_diagnostic(decoded: &str, text_start: usize) -> Option<Diagnostic> {
    if !decoded.starts_with('&') {
        return None;
    }
    Some(Diagnostic {
        code: DiagCode::InvalidEntity,
        span: Some(ByteSpan {
            start: text_start as u32,
            end: text_start as u32 + 1,
        }),
        message: "dangling '&' without a terminating ';' is not a well-formed entity reference; kept as literal text".to_string(),
    })
}

/// B46 (cold code review): resolves one `Event::GeneralRef`'s raw bytes
/// (`&name;` / `&#NN;` / `&#xNN;`), shared by `capture_body`'s in-body arm
/// and the mapper-level/pre-root loops in `parse_str`/`build_mapper`.
/// Returns the decoded text (falls back to the raw, still-escaped text on
/// failure -- degrade gracefully rather than dropping content) plus a
/// diagnostic exactly when the reference is *unresolvable* -- an unknown
/// named reference (e.g. `&nbsp;`) or an invalid numeric one (e.g. an
/// unpaired UTF-16 surrogate codepoint like `&#xD800;`). A resolvable
/// reference (`&amp;`, `&#65;`) never gets a diagnostic here, at any layer:
/// this is the single source of truth both call sites share so "resolvable
/// vs. not" can't drift between them.
fn resolve_general_ref(raw_text: &str, raw_span: ByteSpan) -> (String, Option<Diagnostic>) {
    match quick_xml::escape::unescape(raw_text) {
        Ok(resolved) => (resolved.into_owned(), None),
        Err(err) => {
            let diag = Diagnostic {
                code: DiagCode::InvalidEntity,
                span: Some(raw_span),
                message: format!("unresolvable entity reference: {err}"),
            };
            (raw_text.to_string(), Some(diag))
        }
    }
}

/// MM-13: classifies a reader error for a recovery diagnostic. No new
/// `DiagCode` is warranted for this — `UnclosedTag` already covers "the
/// tag structure around here is broken"; only the message differs between
/// recovery rule 2 (orphan/mismatched closing tag — ignored, parsing
/// continues) and rule 4 (other malformed markup — quick-xml's tokenizer
/// has already resynchronized to the next recognizable token by the time
/// it returns the error).
fn recovery_diagnostic(err: &quick_xml::Error, pos: u64) -> Diagnostic {
    let is_orphan_close = matches!(
        err,
        quick_xml::Error::IllFormed(
            quick_xml::errors::IllFormedError::UnmatchedEndTag(_)
                | quick_xml::errors::IllFormedError::MismatchedEndTag { .. }
        )
    );
    let message = if is_orphan_close {
        format!("orphan or mismatched closing tag ignored: {err}")
    } else {
        format!("malformed markup skipped, resynchronizing: {err}")
    };
    unclosed_tag(pos as usize, pos as usize, message)
}

/// MM-08: one run of decoded text plus the raw (still-escaped) byte span it
/// came from. Per invariant 4, `raw_span` slices back to the *original*
/// bytes (entities/CDATA markers unresolved); `decoded` is the resolved
/// value. The two coincide only when the segment has no entities.
pub(crate) struct TextSegment {
    pub(crate) decoded: String,
    pub(crate) raw_span: ByteSpan,
}

/// One unit of a statement body, in document order. Dynamic tags
/// (`<if>`, `<choose>`, iBatis `<isNotEmpty>`, ...) are recorded as opaque
/// markers — MM-06 will re-walk their span to flatten branches; MM-08 only
/// needs to not let them break text-segment ordering or span accounting.
pub(crate) enum BodySegment {
    Text(TextSegment),
    DynamicTag { name: String, span: ByteSpan },
}

/// Walks a statement body (from just after its own `Start` event to its
/// matching `End`), collecting [`BodySegment`]s. Returns `true` in the
/// third slot when input was truncated (EOF/parse error) — the caller
/// should stop rather than keep reading a reader that already gave up.
pub(crate) fn capture_body(
    source: &str,
    reader: &mut Reader<&[u8]>,
    tag_start: usize,
) -> (Vec<BodySegment>, Vec<Diagnostic>, bool) {
    let mut segments = Vec::new();
    let mut diagnostics = Vec::new();
    let mut last_err_pos = None;

    loop {
        let child_start = reader.buffer_position();
        match reader.read_event() {
            Ok(Event::End(_)) => return (segments, diagnostics, false),
            Ok(Event::Eof) => {
                diagnostics.push(unclosed_tag(
                    tag_start,
                    source.len(),
                    "statement body was never closed",
                ));
                return (segments, diagnostics, true);
            }
            Err(err) => {
                let pos = reader.error_position();
                // Recovery rules 2/4 — see parse_str's Err arm.
                if last_err_pos == Some(pos) {
                    diagnostics.push(unclosed_tag(
                        pos as usize,
                        source.len(),
                        format!("XML parse error in statement body (unrecoverable): {err}"),
                    ));
                    return (segments, diagnostics, true);
                }
                last_err_pos = Some(pos);
                diagnostics.push(recovery_diagnostic(&err, pos));
            }
            Ok(Event::Text(text)) => {
                let end = reader.buffer_position() as usize;
                let raw_span = ByteSpan {
                    start: child_start as u32,
                    end: end as u32,
                };
                // A10 (cold code review): quick-xml 0.41 removed
                // `BytesText::unescape()`. `source` is always
                // already-decoded UTF-8 text (see `encoding.rs`), so
                // `decode()` (encoding only, not entity resolution) can't
                // meaningfully fail here -- and a well-formed `&...;`
                // reference never appears in a `Text` event's content
                // (entity/character references are their own
                // `Event::GeneralRef`, handled separately below).
                let decoded = text.decode().map(|d| d.into_owned()).unwrap_or_else(|_| {
                    source[raw_span.start as usize..raw_span.end as usize].to_string()
                });
                // A18 (cold code review): with `allow_dangling_amp` enabled
                // (see parse_str), a bare `&` that isn't part of a
                // well-formed reference is delivered as literal `Text`
                // rather than an error -- and quick-xml always starts a
                // fresh `Text` event exactly at such a dangling `&` (never
                // mid-run), so a leading `&` here unambiguously means this
                // whole segment is one. Diagnose it the same way a
                // malformed *resolved* reference already is (see the
                // `GeneralRef` arm below) -- MM-08's raw-text-kept-verbatim
                // rule means the SQL text, and any placeholder later in the
                // same run, still survives untouched.
                // B43/B44 (cold code review): span narrowed to exactly
                // the 1-byte `&` (see `dangling_amp_diagnostic`'s doc
                // comment) -- the whole-Text-event span used to swallow
                // dozens of bytes of perfectly ordinary SQL text (the
                // shipped `bare_ampersand_in_text` fixture) into an
                // "invalid entity" span. Shared with the mapper-level
                // checks in `parse_str`/`build_mapper` (B44) so a
                // dangling amp is diagnosed identically regardless of
                // which layer of the tree it's in.
                if let Some(diag) = dangling_amp_diagnostic(&decoded, raw_span.start as usize) {
                    diagnostics.push(diag);
                }
                segments.push(BodySegment::Text(TextSegment { decoded, raw_span }));
            }
            Ok(Event::GeneralRef(_entity_ref)) => {
                // A10 (cold code review): as of quick-xml 0.41, an entity
                // or character reference (`&name;` / `&#NN;` / `&#xNN;`)
                // is its own event rather than embedded in the
                // surrounding `Event::Text`'s raw content -- this is the
                // direct replacement for the pre-0.41 whole-blob
                // `BytesText::unescape()` call, just resolved one
                // reference at a time. Produces its own `TextSegment`
                // (adjacent segments are merged before MM-07 placeholder
                // normalization -- see flatten.rs's B16 comment), so
                // surrounding plain text keeps an exact (verbatim)
                // span_map entry instead of the whole run being coarsened
                // just because one reference in it needed resolving.
                let end = reader.buffer_position() as usize;
                let raw_span = ByteSpan {
                    start: child_start as u32,
                    end: end as u32,
                };
                let raw_text = &source[raw_span.start as usize..raw_span.end as usize];
                // B46: resolution + diagnostic logic now shared with the
                // mapper-level/pre-root loops via `resolve_general_ref` --
                // see its doc comment for what counts as "unresolvable".
                let (decoded, diag) = resolve_general_ref(raw_text, raw_span);
                if let Some(diag) = diag {
                    diagnostics.push(diag);
                }
                segments.push(BodySegment::Text(TextSegment { decoded, raw_span }));
            }
            Ok(Event::CData(cdata)) => {
                let end = reader.buffer_position() as usize;
                // The event's own span includes the `<![CDATA[`/`]]>`
                // delimiters (9 and 3 bytes); the segment span is the
                // inner content only.
                let inner_start = child_start as usize + 9;
                let inner_end = end - 3;
                let raw_span = ByteSpan {
                    start: inner_start as u32,
                    end: inner_end as u32,
                };
                let decoded = match cdata.decode() {
                    Ok(decoded) => decoded.into_owned(),
                    Err(_) => source[inner_start..inner_end].to_string(),
                };
                segments.push(BodySegment::Text(TextSegment { decoded, raw_span }));
            }
            Ok(Event::Start(tag)) => {
                let name = String::from_utf8_lossy(tag.local_name().as_ref()).into_owned();
                match skip_subtree(reader, &mut diagnostics) {
                    SkipOutcome::Closed => {
                        let end = reader.buffer_position();
                        segments.push(BodySegment::DynamicTag {
                            name,
                            span: ByteSpan {
                                start: child_start as u32,
                                end: end as u32,
                            },
                        });
                    }
                    SkipOutcome::Eof => {
                        diagnostics.push(unclosed_tag(
                            child_start as usize,
                            source.len(),
                            format!("<{name}> was never closed"),
                        ));
                        return (segments, diagnostics, true);
                    }
                    SkipOutcome::Err(err) => {
                        diagnostics.push(unclosed_tag(
                            child_start as usize,
                            source.len(),
                            format!("XML parse error while skipping <{name}>: {err}"),
                        ));
                        return (segments, diagnostics, true);
                    }
                }
            }
            Ok(Event::Empty(tag)) => {
                let end = reader.buffer_position();
                let name = String::from_utf8_lossy(tag.local_name().as_ref()).into_owned();
                segments.push(BodySegment::DynamicTag {
                    name,
                    span: ByteSpan {
                        start: child_start as u32,
                        end: end as u32,
                    },
                });
            }
            _ => continue,
        }
    }
}

/// MM-06: re-parses a `DynamicTag` marker's own span independently — the
/// shared reader used for the statement/fragment's top-level walk has
/// already moved past it via `skip_subtree`, so flattening re-parses from a
/// fresh `Reader` over the marker's byte slice. Every returned span (in
/// segments and diagnostics) is shifted back into `full_source`'s
/// coordinate system before returning.
pub(crate) fn capture_subtree(
    full_source: &str,
    span: ByteSpan,
) -> (Vec<BodySegment>, Vec<Diagnostic>, bool) {
    let sub = &full_source[span.start as usize..span.end as usize];
    let mut reader = Reader::from_str(sub);
    // A18: see parse_str's identical setting -- this is a fresh `Reader`
    // over the marker's own byte slice, so it doesn't inherit the outer
    // reader's config and needs the setting again.
    reader.config_mut().allow_dangling_amp = true;
    match reader.read_event() {
        Ok(Event::Empty(_)) => (Vec::new(), Vec::new(), false),
        Ok(Event::Start(_)) => {
            let (segments, diagnostics, truncated) = capture_body(sub, &mut reader, 0);
            (
                shift_segments(segments, span.start),
                shift_diagnostics(diagnostics, span.start),
                truncated,
            )
        }
        // The span was already parsed once by the caller when this marker
        // was first found, so this shouldn't happen — but no panics on
        // public paths, so just report nothing rather than unwrap.
        _ => (Vec::new(), Vec::new(), false),
    }
}

fn shift_span(span: ByteSpan, offset: u32) -> ByteSpan {
    ByteSpan {
        start: span.start + offset,
        end: span.end + offset,
    }
}

fn shift_segments(segments: Vec<BodySegment>, offset: u32) -> Vec<BodySegment> {
    segments
        .into_iter()
        .map(|s| match s {
            BodySegment::Text(t) => BodySegment::Text(TextSegment {
                decoded: t.decoded,
                raw_span: shift_span(t.raw_span, offset),
            }),
            BodySegment::DynamicTag { name, span } => BodySegment::DynamicTag {
                name,
                span: shift_span(span, offset),
            },
        })
        .collect()
}

fn shift_diagnostics(diagnostics: Vec<Diagnostic>, offset: u32) -> Vec<Diagnostic> {
    diagnostics
        .into_iter()
        .map(|d| Diagnostic {
            code: d.code,
            span: d.span.map(|s| shift_span(s, offset)),
            message: d.message,
        })
        .collect()
}

/// Returns the first `name="value"` match (raw, span-preserving) plus a
/// `DuplicateAttribute` diagnostic for every repeat (recovery rule 3:
/// first value wins).
pub(crate) fn attr_value_spanned(
    source: &str,
    attrs: &[RawAttr],
    name: &[u8],
) -> (Option<Spanned<String>>, Vec<Diagnostic>) {
    let bytes = source.as_bytes();
    let mut matches = attrs.iter().filter(|a| &bytes[a.name.0..a.name.1] == name);

    let first = matches.next().map(|attr| Spanned {
        value: source[attr.value.0..attr.value.1].to_string(),
        span: ByteSpan {
            start: attr.value.0 as u32,
            end: attr.value.1 as u32,
        },
    });

    let diagnostics = matches
        .map(|dup| Diagnostic {
            code: DiagCode::DuplicateAttribute,
            span: Some(ByteSpan {
                start: dup.name.0 as u32,
                end: dup.name.1 as u32,
            }),
            message: format!(
                "duplicate '{}' attribute; first value wins",
                String::from_utf8_lossy(name)
            ),
        })
        .collect();

    (first, diagnostics)
}

/// A raw `name="value"` pair as byte ranges into the original source
/// (`name` and `value` each exclude quotes/`=`).
pub(crate) struct RawAttr {
    name: (usize, usize),
    value: (usize, usize),
}

/// Tokenizes a tag's raw byte range `[tag_start, tag_end)` into its
/// attributes: skip whitespace, read a name, `=`, then consume a quoted
/// value *as a whole unit* (whatever it contains). Consuming the value
/// wholesale — rather than scanning for the next occurrence of a name byte
/// by byte — is what keeps an attribute name that happens to appear inside
/// another attribute's quoted value (e.g. a `'...' `-quoted value
/// containing a literal `"`) from being mistaken for a real attribute.
pub(crate) fn scan_attributes(bytes: &[u8], tag_start: usize, tag_end: usize) -> Vec<RawAttr> {
    let mut attrs = Vec::new();
    let mut i = tag_start;

    // Skip `<` and the element name.
    if i < tag_end && bytes[i] == b'<' {
        i += 1;
    }
    while i < tag_end && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' && bytes[i] != b'/' {
        i += 1;
    }

    loop {
        while i < tag_end && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag_end || bytes[i] == b'>' || bytes[i] == b'/' {
            break;
        }

        let name_start = i;
        // Cold code review B6: must also stop at `>`/`/` (the tag's own
        // end), not just `=`/whitespace -- otherwise a bare valueless
        // attribute immediately followed by `>` (e.g. `<if foo>x AND
        // test = "1"</if>`) reads straight through the opening tag's `>`
        // into the body text, fabricating a "test" attribute from SQL
        // that was never inside the tag at all.
        while i < tag_end
            && bytes[i] != b'='
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'>'
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let name_end = i;

        while i < tag_end && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag_end || bytes[i] != b'=' {
            // MM-13: a bare valueless attribute (e.g. legacy HTML-ism, or
            // just malformed markup) — skip it and keep scanning for real
            // attributes rather than abandoning the whole tag's worth of
            // them. `i` is already past the bare name, at the start of
            // whatever comes next.
            continue;
        }
        i += 1;
        while i < tag_end && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let Some(&quote) = bytes.get(i).filter(|b| **b == b'"' || **b == b'\'') else {
            break;
        };
        i += 1;
        let value_start = i;
        while i < tag_end && bytes[i] != quote {
            i += 1;
        }
        if i >= tag_end {
            break; // unterminated attribute value
        }
        let value_end = i;
        i += 1; // consume closing quote

        attrs.push(RawAttr {
            name: (name_start, name_end),
            value: (value_start, value_end),
        });
    }

    attrs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_01_mapper_root_is_mybatis_dialect() {
        let source = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE mapper PUBLIC "-//mybatis.org//DTD Mapper 3.0//EN"
  "http://mybatis.org/dtd/mybatis-3-mapper.dtd">
<mapper namespace="com.example.demo.mapper.WidgetMapper">
</mapper>"#;
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Mybatis);
        assert!(result.mapper.is_some());
    }

    #[test]
    fn mm_01_sqlmap_root_is_ibatis_dialect() {
        let source = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE sqlMap PUBLIC "-//ibatis.apache.org//DTD SQL Map 2.0//EN"
  "http://ibatis.apache.org/dtd/sql-map-2.dtd">
<sqlMap>
</sqlMap>"#;
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Ibatis);
        assert!(result.mapper.is_some());
    }

    #[test]
    fn mm_01_leading_comment_before_root_is_skipped() {
        let source =
            "<!-- generated by legacy codegen, do not edit -->\n<mapper namespace=\"x\"></mapper>";
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Mybatis);
        assert!(result.mapper.is_some());
    }

    #[test]
    fn mm_01_bom_before_root_is_skipped() {
        // A19 (cold code review): strengthened beyond dialect/mapper
        // presence to also assert id, text, and span slices -- a BOM/
        // reader-position skew corrupted exactly these (garbled text, a
        // lost statement id) without ever flipping `dialect` or
        // `mapper.is_some()`, so those two alone didn't catch it.
        let with_bom =
            "\u{FEFF}<mapper namespace=\"x\"><select id=\"a\">SELECT 1</select></mapper>";
        let without_bom = "<mapper namespace=\"x\"><select id=\"a\">SELECT 1</select></mapper>";
        let result = parse_str(with_bom);
        assert_eq!(result.dialect, Dialect::Mybatis);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.namespace.as_ref().unwrap().value, "x");
        assert_eq!(mapper.statements.len(), 1);
        let stmt = &mapper.statements[0];
        assert_eq!(stmt.id.as_ref().unwrap().value, "a");
        let SqlText::Variants(variants) = &stmt.sql else {
            panic!("expected a single unconditional variant")
        };
        assert_eq!(variants[0].text.text, "SELECT 1");

        // Spans must match a parse of the same document *without* a BOM,
        // byte-for-byte -- i.e. every offset is relative to the
        // BOM-stripped content, never the original BOM-prefixed input
        // (see parse_str's doc comment and `ParseResult::encoding`'s).
        let without_bom_result = parse_str(without_bom);
        let expected_mapper = without_bom_result.mapper.expect("mapper root");
        assert_eq!(
            mapper.namespace.as_ref().unwrap().span,
            expected_mapper.namespace.as_ref().unwrap().span
        );
        assert_eq!(stmt.span, expected_mapper.statements[0].span);
        assert_eq!(
            stmt.id.as_ref().unwrap().span,
            expected_mapper.statements[0].id.as_ref().unwrap().span
        );
    }

    /// Shared assertion body for A21's double/triple leading BOM tests:
    /// `parse_str` on `with_boms` must agree byte-for-byte with a plain
    /// parse of `without_bom`, and `crate::parse`/`crate::parse_bytes` on
    /// the *same* `with_boms` content must produce byte-identical JSON --
    /// the two public entry points must never diverge on how many BOMs
    /// they strip.
    fn assert_bom_stripping_agrees(with_boms: &str, without_bom: &str) {
        let result = parse_str(with_boms);
        assert_eq!(result.dialect, Dialect::Mybatis);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.namespace.as_ref().unwrap().value, "x");
        assert_eq!(mapper.statements.len(), 1);
        let stmt = &mapper.statements[0];
        assert_eq!(stmt.id.as_ref().unwrap().value, "a");
        let SqlText::Variants(variants) = &stmt.sql else {
            panic!("expected a single unconditional variant")
        };
        assert_eq!(variants[0].text.text, "SELECT 1");

        let without_bom_result = parse_str(without_bom);
        let expected_mapper = without_bom_result.mapper.expect("mapper root");
        assert_eq!(
            mapper.namespace.as_ref().unwrap().span,
            expected_mapper.namespace.as_ref().unwrap().span
        );
        assert_eq!(stmt.span, expected_mapper.statements[0].span);
        assert_eq!(
            stmt.id.as_ref().unwrap().span,
            expected_mapper.statements[0].id.as_ref().unwrap().span
        );

        // A21: parse(&str) and parse_bytes(&[u8]) must agree, not just on
        // dialect/id/text, but byte-for-byte in their full JSON output --
        // the two entry points must strip the same number of BOMs.
        let str_result = crate::parse(with_boms);
        let bytes_result = crate::parse_bytes(with_boms.as_bytes());
        let str_json = serde_json::to_string_pretty(&str_result).expect("ParseResult serializes");
        let bytes_json =
            serde_json::to_string_pretty(&bytes_result).expect("ParseResult serializes");
        assert_eq!(
            str_json, bytes_json,
            "parse() and parse_bytes() must agree byte-for-byte on the same BOM-laden content"
        );
    }

    #[test]
    fn a21_double_leading_bom_is_fully_stripped() {
        // A21 (cold code review): a single `strip_prefix` call only
        // removes one BOM, leaving a second one for quick-xml to skip on
        // its own -- reintroducing the exact 3-byte span skew A19 fixed
        // for a single BOM. This must go through both public entry points
        // identically.
        let with_boms =
            "\u{FEFF}\u{FEFF}<mapper namespace=\"x\"><select id=\"a\">SELECT 1</select></mapper>";
        let without_bom = "<mapper namespace=\"x\"><select id=\"a\">SELECT 1</select></mapper>";
        assert_bom_stripping_agrees(with_boms, without_bom);
    }

    #[test]
    fn a21_triple_leading_bom_is_fully_stripped() {
        // Same as the double-BOM case, but with three leading BOMs, to
        // confirm the loop (not just a second manual strip) is correct.
        let with_boms = "\u{FEFF}\u{FEFF}\u{FEFF}<mapper namespace=\"x\"><select id=\"a\">SELECT 1</select></mapper>";
        let without_bom = "<mapper namespace=\"x\"><select id=\"a\">SELECT 1</select></mapper>";
        assert_bom_stripping_agrees(with_boms, without_bom);
    }

    #[test]
    fn a21_mid_text_bom_is_preserved_verbatim() {
        // Regression guard: A21's fix only strips *leading* BOMs (via
        // `strip_prefix` in a loop). A U+FEFF appearing mid-document --
        // e.g. accidentally pasted into SQL text -- is ordinary content,
        // not a byte-order mark, and must survive untouched. This must
        // not be conflated with the leading-BOM-stripping loop above.
        let source = "<mapper namespace=\"x\"><select id=\"a\">SELECT '\u{FEFF}' FROM dual</select></mapper>";
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Mybatis);
        let mapper = result.mapper.expect("mapper root");
        let stmt = &mapper.statements[0];
        let SqlText::Variants(variants) = &stmt.sql else {
            panic!("expected a single unconditional variant")
        };
        assert!(
            variants[0].text.text.contains('\u{FEFF}'),
            "mid-text BOM must be preserved verbatim, got: {:?}",
            variants[0].text.text
        );
        assert_eq!(variants[0].text.text, "SELECT '\u{FEFF}' FROM dual");
    }

    #[test]
    fn a21_parse_bytes_strips_n_raw_utf8_boms() {
        // A21: `encoding::decode` strips exactly one raw UTF-8 BOM
        // (`[0xEF, 0xBB, 0xBF]`) before handing off to `parse_str`, which
        // now loops to strip any further leading BOMs itself. Exercise
        // `crate::parse_bytes` directly with N *raw byte* BOMs (as
        // opposed to pre-decoded `\u{FEFF}` chars in a `&str`) for
        // N = 1, 2, 3 to confirm the two-stage stripping composes
        // correctly regardless of which layer strips how many.
        const UTF8_BOM_BYTES: [u8; 3] = [0xEF, 0xBB, 0xBF];
        let body = b"<mapper namespace=\"x\"><select id=\"a\">SELECT 1</select></mapper>";
        let expected = crate::parse_bytes(body);
        let expected_mapper = expected.mapper.expect("mapper root");

        for n in 1..=3 {
            let mut bytes = Vec::new();
            for _ in 0..n {
                bytes.extend_from_slice(&UTF8_BOM_BYTES);
            }
            bytes.extend_from_slice(body);
            let result = crate::parse_bytes(&bytes);
            assert_eq!(result.dialect, Dialect::Mybatis, "n={n}");
            let mapper = result
                .mapper
                .unwrap_or_else(|| panic!("mapper root, n={n}"));
            assert_eq!(mapper.namespace.as_ref().unwrap().value, "x", "n={n}");
            assert_eq!(
                mapper.namespace.as_ref().unwrap().span,
                expected_mapper.namespace.as_ref().unwrap().span,
                "n={n}"
            );
            let stmt = &mapper.statements[0];
            assert_eq!(stmt.id.as_ref().unwrap().value, "a", "n={n}");
            assert_eq!(stmt.span, expected_mapper.statements[0].span, "n={n}");
            let SqlText::Variants(variants) = &stmt.sql else {
                panic!("expected a single unconditional variant, n={n}")
            };
            assert_eq!(variants[0].text.text, "SELECT 1", "n={n}");
        }
    }

    #[test]
    fn mm_01_configuration_root_yields_no_mapper() {
        let source = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE configuration PUBLIC "-//mybatis.org//DTD Config 3.0//EN"
  "http://mybatis.org/dtd/mybatis-3-config.dtd">
<configuration>
</configuration>"#;
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Unknown);
        assert!(result.mapper.is_none());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, DiagCode::UnknownElement);
        assert!(result.diagnostics[0].span.is_some());
    }

    #[test]
    fn mm_01_malformed_input_yields_diagnostic_not_silence() {
        let source = "<mapper namespace=\"x\"";
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Unknown);
        assert!(result.mapper.is_none());
        // MM-13: the reader error is now recoverable (doesn't abort
        // immediately), so parsing continues to genuine EOF and diagnoses
        // that too — two diagnostics, not a silent single failure.
        assert_eq!(result.diagnostics.len(), 2);
        assert_eq!(result.diagnostics[0].code, DiagCode::UnclosedTag);
        assert_eq!(result.diagnostics[1].code, DiagCode::UnknownElement);
    }

    #[test]
    fn mm_02_namespace_attribute_is_captured_with_span() {
        let source = r#"<mapper namespace="com.example.demo.mapper.WidgetMapper"></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let namespace = mapper.namespace.expect("namespace present");
        assert_eq!(namespace.value, "com.example.demo.mapper.WidgetMapper");
        let ByteSpan { start, end } = namespace.span;
        assert_eq!(&source[start as usize..end as usize], namespace.value);
    }

    #[test]
    fn mm_02_missing_namespace_is_none() {
        // iBatis no-namespace mode: the prefix lives inside the statement
        // id, not a namespace attribute.
        let source = "<sqlMap></sqlMap>";
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert!(mapper.namespace.is_none());
    }

    #[test]
    fn mm_02_namespace_with_embedded_whitespace_and_newline() {
        let source = "<mapper namespace=\"com.example\n  .demo.Mapper\"></mapper>";
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let namespace = mapper.namespace.expect("namespace present");
        assert_eq!(namespace.value, "com.example\n  .demo.Mapper");
        let ByteSpan { start, end } = namespace.span;
        assert_eq!(&source[start as usize..end as usize], namespace.value);
    }

    #[test]
    fn mm_02_empty_namespace_is_some_empty_string() {
        let source = r#"<mapper namespace=""></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let namespace = mapper.namespace.expect("namespace attribute present");
        assert_eq!(namespace.value, "");
        assert_eq!(namespace.span.start, namespace.span.end);
    }

    #[test]
    fn mm_02_empty_element_root_with_attribute() {
        let source = r#"<mapper namespace="com.example.demo.Mapper"/>"#;
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Mybatis);
        let mapper = result.mapper.expect("mapper root");
        let namespace = mapper.namespace.expect("namespace present");
        assert_eq!(namespace.value, "com.example.demo.Mapper");
        let ByteSpan { start, end } = namespace.span;
        assert_eq!(&source[start as usize..end as usize], namespace.value);
    }

    #[test]
    fn mm_02_attribute_name_inside_other_quoted_value_is_not_a_false_match() {
        // Single-quoted attribute values may legally contain literal `"`
        // characters; a naive byte scan for `namespace=` can wander into
        // this value and misfire.
        let source = r#"<mapper other='see namespace="wrong"' namespace="real"></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let namespace = mapper.namespace.expect("namespace present");
        assert_eq!(namespace.value, "real");
        let ByteSpan { start, end } = namespace.span;
        assert_eq!(&source[start as usize..end as usize], namespace.value);
    }

    #[test]
    fn mm_02_duplicate_namespace_first_value_wins_with_diagnostic() {
        let source = r#"<mapper namespace="a" namespace="b"></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let namespace = mapper.namespace.expect("namespace present");
        assert_eq!(namespace.value, "a");
        let ByteSpan { start, end } = namespace.span;
        assert_eq!(&source[start as usize..end as usize], namespace.value);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, DiagCode::DuplicateAttribute);
    }

    #[test]
    fn mm_03_select_statement_is_collected() {
        let source =
            r#"<mapper namespace="x"><select id="selectWidget">SELECT 1</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        let stmt = &mapper.statements[0];
        assert_eq!(stmt.kind, StatementKind::Select);
        let id = stmt.id.as_ref().expect("id present");
        assert_eq!(id.value, "selectWidget");
        let ByteSpan { start, end } = id.span;
        assert_eq!(&source[start as usize..end as usize], id.value);
    }

    #[test]
    fn mm_03_multiple_statement_kinds_collected_in_order() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1</select>
            <insert id="b">INSERT 1</insert>
            <update id="c">UPDATE 1</update>
            <delete id="d">DELETE 1</delete>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let kinds: Vec<_> = mapper.statements.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                StatementKind::Select,
                StatementKind::Insert,
                StatementKind::Update,
                StatementKind::Delete,
            ]
        );
        let ids: Vec<_> = mapper
            .statements
            .iter()
            .map(|s| s.id.as_ref().unwrap().value.clone())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn mm_03_ibatis_procedure_and_generic_statement_tags() {
        let source = r#"<sqlMap>
            <procedure id="callProc">{call proc()}</procedure>
            <statement id="genericOne">SELECT 1</statement>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let kinds: Vec<_> = mapper.statements.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![StatementKind::Procedure, StatementKind::Generic]
        );
    }

    #[test]
    fn mm_03_missing_id_yields_diagnostic() {
        let source = r#"<mapper namespace="x"><select>SELECT 1</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        assert!(mapper.statements[0].id.is_none());
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::MissingStatementId));
    }

    #[test]
    fn mm_03_duplicate_statement_id_both_preserved_with_diagnostic() {
        let source = r#"<mapper namespace="x">
            <select id="dup">SELECT 1</select>
            <select id="dup">SELECT 2</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 2);
        assert_eq!(mapper.statements[0].id.as_ref().unwrap().value, "dup");
        assert_eq!(mapper.statements[1].id.as_ref().unwrap().value, "dup");
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|d| d.code == DiagCode::DuplicateStatementId)
                .count(),
            1
        );
    }

    #[test]
    fn mm_03_database_id_branch_is_not_flagged_as_duplicate() {
        let source = r#"<mapper namespace="x">
            <select id="dup" databaseId="oracle">SELECT 1 FROM dual</select>
            <select id="dup" databaseId="mysql">SELECT 1</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 2);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DuplicateStatementId));
    }

    #[test]
    fn mm_03_nested_dynamic_tags_do_not_break_statement_boundary() {
        let source = r#"<mapper namespace="x">
            <select id="withIf">
                SELECT 1
                <if test="a != null">
                    <choose>
                        <when test="b">AND b = #{b}</when>
                    </choose>
                </if>
            </select>
            <select id="afterNesting">SELECT 2</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let ids: Vec<_> = mapper
            .statements
            .iter()
            .map(|s| s.id.as_ref().unwrap().value.clone())
            .collect();
        assert_eq!(ids, vec!["withIf", "afterNesting"]);
    }

    #[test]
    fn mm_03_ibatis_embedded_prefix_id_survives_unsplit() {
        // iBatis no-namespace mode: the "namespace" lives inside the
        // statement id itself (e.g. `WidgetDAO.getWidget`), not as a
        // separate attribute — must not be split or reinterpreted.
        let source = r#"<sqlMap><select id="WidgetDAO.getWidget">SELECT 1</select></sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        let id = mapper.statements[0].id.as_ref().expect("id present");
        assert_eq!(id.value, "WidgetDAO.getWidget");
        let ByteSpan { start, end } = id.span;
        assert_eq!(&source[start as usize..end as usize], id.value);
    }

    #[test]
    fn mm_03_reader_error_mid_collection_reports_diagnostic_not_silence() {
        // The lone `</wrongclose>` doesn't match any open element (mapper
        // is still open) — quick-xml's check_end_names rejects it, and the
        // top-level read_event() in build_mapper's loop returns Err
        // directly (distinct from the SkipOutcome::Err path below).
        let source =
            r#"<mapper namespace="x"><select id="ok">SELECT 1</select></wrongclose></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        assert_eq!(mapper.statements[0].id.as_ref().unwrap().value, "ok");
        assert!(!result.diagnostics.is_empty());
    }

    #[test]
    fn mm_03_reader_error_while_skipping_subtree_reports_diagnostic_not_silence() {
        // The second statement's own body is malformed (mismatched end
        // tag), so the error surfaces from skip_subtree rather than the
        // top-level read_event() — a separate code path from the test
        // above, exercised here directly.
        let source = r#"<mapper namespace="x"><select id="ok">SELECT 1</select><insert id="bad">INSERT 1</delete></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert!(!mapper.statements.is_empty());
        assert!(!result.diagnostics.is_empty());
    }

    #[test]
    fn mm_03_full_statement_collection_snapshot() {
        let source = r#"<mapper namespace="com.example.WidgetMapper">
            <select id="selectWidget" databaseId="oracle">SELECT 1 FROM dual</select>
            <select id="selectWidget" databaseId="mysql">SELECT 1</select>
            <insert id="insertWidget">INSERT INTO widget VALUES (#{id})</insert>
            <select id="selectWidget" databaseId="oracle">DUPLICATE BRANCH</select>
        </mapper>"#;
        let result = parse_str(source);
        insta::assert_json_snapshot!(result);
    }

    // A6 (cold code review, major): <selectKey> used to be flattened as a
    // transparent passthrough dynamic tag, concatenating its body straight
    // into the parent statement's SQL. It must now split into its own
    // Statement (kind Select, id parent_id + "!selectKey", own span),
    // excluded entirely from the parent's SQL.

    #[test]
    fn a6_mybatis_select_key_splits_into_its_own_statement() {
        let source = r#"<mapper namespace="x">
            <insert id="insertWidget">
                <selectKey keyProperty="id" resultType="long" order="BEFORE">SELECT NEXT VALUE FOR widget_seq</selectKey>
                INSERT INTO widget (id, name) VALUES (#{id}, #{name})
            </insert>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 2);

        let parent = &mapper.statements[0];
        assert_eq!(parent.id.as_ref().unwrap().value, "insertWidget");
        let parent_vs = variants(&parent.sql);
        assert_eq!(parent_vs.len(), 1);
        assert!(!parent_vs[0].text.text.contains("SELECT NEXT VALUE"));
        assert!(parent_vs[0].text.text.contains("INSERT INTO widget"));

        let child = &mapper.statements[1];
        assert_eq!(child.id.as_ref().unwrap().value, "insertWidget!selectKey");
        assert_eq!(child.kind, StatementKind::Select);
        assert_eq!(child.result_class.as_ref().unwrap().value.raw, "long");
        let child_vs = variants(&child.sql);
        assert_eq!(child_vs.len(), 1);
        assert_eq!(child_vs[0].text.text, "SELECT NEXT VALUE FOR widget_seq");
        // Own span: covers just the <selectKey> element, nested inside (not
        // equal to) the parent's full extent.
        assert!(child.span.start > parent.span.start);
        assert!(child.span.end < parent.span.end);
    }

    #[test]
    fn a6_ibatis_select_key_splits_into_its_own_statement() {
        let source = r#"<sqlMap>
            <insert id="widgetDAO.insertWidget" parameterClass="widget">
                <selectKey keyProperty="id" resultClass="long">SELECT NEXT VALUE FOR widget_seq</selectKey>
                INSERT INTO widget (id, name) VALUES (#id#, #name#)
            </insert>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 2);

        let child = &mapper.statements[1];
        assert_eq!(
            child.id.as_ref().unwrap().value,
            "widgetDAO.insertWidget!selectKey"
        );
        assert_eq!(child.kind, StatementKind::Select);
        let child_vs = variants(&child.sql);
        assert_eq!(child_vs[0].text.text, "SELECT NEXT VALUE FOR widget_seq");

        let parent_vs = variants(&mapper.statements[0].sql);
        assert!(!parent_vs[0].text.text.contains("SELECT NEXT VALUE"));
    }

    #[test]
    fn a6_select_key_containing_placeholder_normalizes_and_records_property_path() {
        let source = r#"<mapper namespace="x">
            <insert id="insertWidget">
                <selectKey keyProperty="id" resultType="long">SELECT seq_next(#{prefix})</selectKey>
                INSERT INTO widget (id) VALUES (#{id})
            </insert>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let child = &mapper.statements[1];
        let child_vs = variants(&child.sql);
        assert_eq!(child_vs[0].text.text, "SELECT seq_next(?)");
        assert_eq!(child.property_paths.len(), 1);
        assert_eq!(child.property_paths[0].value, "prefix");

        // The parent's own property_paths cover only its own placeholder,
        // not the (now excluded) selectKey body's.
        let parent = &mapper.statements[0];
        assert_eq!(parent.property_paths.len(), 1);
        assert_eq!(parent.property_paths[0].value, "id");
    }

    #[test]
    fn b27_select_key_at_top_level_of_sql_fragment_is_diagnosed_and_dropped() {
        // Cold code review B27: <selectKey> only makes sense as a direct
        // child of <insert>/<update> (see A6) -- a <sql> fragment has no
        // MappedStatement to synthesize a child onto, so it used to just
        // silently mash the selectKey's body into the fragment's own SQL
        // text. Must now be dropped entirely (not folded in) with a
        // diagnostic naming the actual problem (invalid placement), not
        // A14's generic "unrecognized element" message.
        let source = r#"<mapper namespace="x">
            <sql id="frag"><selectKey keyProperty="id" resultType="long">SELECT 1</selectKey>a = 1</sql>
            <select id="s"><include refid="frag"/></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let fragment_vs = variants(&mapper.fragments[0].sql);
        assert_eq!(
            fragment_vs[0].text.text, "a = 1",
            "the selectKey body must be dropped, not mashed into the fragment text"
        );
        let unknown: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::UnknownElement)
            .collect();
        assert_eq!(
            unknown.len(),
            1,
            "exactly one diagnostic, not A14's generic one too"
        );
        assert!(unknown[0]
            .message
            .contains("not valid inside a <sql> fragment"));
    }

    #[test]
    fn a13_select_key_reads_its_own_database_id() {
        // Cold code review A13 (major): build_select_key_statement used to
        // hardcode database_id: None instead of reading the attribute like
        // every other statement-like tag.
        let source = r#"<mapper namespace="x">
            <insert id="insertWidget">
                <selectKey keyProperty="id" resultType="long" databaseId="oracle">SELECT widget_seq.nextval FROM dual</selectKey>
                INSERT INTO widget (id) VALUES (#{id})
            </insert>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let child = &mapper.statements[1];
        assert_eq!(child.database_id.as_ref().unwrap().value, "oracle");
    }

    #[test]
    fn a13_two_select_keys_with_different_database_ids_are_not_duplicates() {
        // A13: legitimate MyBatis databaseId branching -- two selectKeys
        // synthesizing the *same* id ("insertWidget!selectKey") but with
        // different databaseIds must not be flagged, exactly like two real
        // statements sharing an id under different databaseIds.
        let source = r#"<mapper namespace="x">
            <insert id="insertWidget">
                <selectKey keyProperty="id" resultType="long" databaseId="oracle">SELECT widget_seq.nextval FROM dual</selectKey>
                <selectKey keyProperty="id" resultType="long" databaseId="mysql">SELECT LAST_INSERT_ID()</selectKey>
                INSERT INTO widget (id) VALUES (#{id})
            </insert>
        </mapper>"#;
        let result = parse_str(source);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DuplicateStatementId));
    }

    #[test]
    fn a13_two_select_keys_with_the_same_database_id_are_duplicates() {
        // A13: same synthesized id, same (absent) databaseId -- a genuine
        // collision that must be reported, just like two real statements
        // sharing an id with no databaseId at all.
        let source = r#"<mapper namespace="x">
            <insert id="insertWidget">
                <selectKey keyProperty="id" resultType="long">SELECT 1</selectKey>
                <selectKey keyProperty="id" resultType="long">SELECT 2</selectKey>
                INSERT INTO widget (id) VALUES (#{id})
            </insert>
        </mapper>"#;
        let result = parse_str(source);
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DuplicateStatementId));
    }

    #[test]
    fn a13_real_statement_colliding_with_a_synthesized_select_key_id_is_a_duplicate() {
        // A13: build_select_key_statement never registered its synthesized
        // id in seen_ids at all, so a real, separately authored statement
        // literally named "insertWidget!selectKey" collided silently with
        // the id synthesized by <insertWidget>'s own <selectKey> child.
        let source = r#"<mapper namespace="x">
            <insert id="insertWidget">
                <selectKey keyProperty="id" resultType="long">SELECT 1</selectKey>
                INSERT INTO widget (id) VALUES (#{id})
            </insert>
            <select id="insertWidget!selectKey">SELECT 2</select>
        </mapper>"#;
        let result = parse_str(source);
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DuplicateStatementId));
    }

    #[test]
    fn a20_select_key_without_its_own_database_id_inherits_the_parents() {
        // Cold code review A20 (major): the canonical dual-dialect
        // pattern -- two <insert>s sharing an id, each with its own
        // databaseId, each carrying a plain <selectKey> with no
        // databaseId of its own -- used to synthesize two
        // "insertWidget!selectKey" statements that both read
        // `database_id: None`, colliding as a spurious duplicate even
        // though the parents are legitimately dialect-branched. Each
        // selectKey must both report its parent's databaseId AND not be
        // flagged.
        let source = r#"<mapper namespace="x">
            <insert id="insertWidget" databaseId="oracle">
                <selectKey keyProperty="id" resultType="long">SELECT widget_seq.nextval FROM dual</selectKey>
                INSERT INTO widget (id) VALUES (#{id})
            </insert>
            <insert id="insertWidget" databaseId="mysql">
                <selectKey keyProperty="id" resultType="long">SELECT LAST_INSERT_ID()</selectKey>
                INSERT INTO widget (id) VALUES (#{id})
            </insert>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        // statements: [insert(oracle), selectKey(oracle), insert(mysql), selectKey(mysql)]
        let oracle_key = &mapper.statements[1];
        let mysql_key = &mapper.statements[3];
        assert_eq!(oracle_key.database_id.as_ref().unwrap().value, "oracle");
        assert_eq!(mysql_key.database_id.as_ref().unwrap().value, "mysql");
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.code == DiagCode::DuplicateStatementId),
            "dialect-branched selectKeys must not collide: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn a20_select_key_s_own_database_id_overrides_the_parent_s() {
        // An explicit databaseId on <selectKey> itself still wins over
        // inheriting the parent's -- inheritance only fills in absence.
        let source = r#"<mapper namespace="x">
            <insert id="insertWidget" databaseId="oracle">
                <selectKey keyProperty="id" resultType="long" databaseId="mysql">SELECT LAST_INSERT_ID()</selectKey>
                INSERT INTO widget (id) VALUES (#{id})
            </insert>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let child = &mapper.statements[1];
        assert_eq!(
            child.database_id.as_ref().unwrap().value,
            "mysql",
            "selectKey's own databaseId must win over the parent's"
        );
    }

    #[test]
    fn a20_two_select_keys_with_the_same_effective_database_id_are_still_duplicates() {
        // Two selectKeys that resolve to the *same* effective databaseId
        // (one inherited, one explicit but matching) is a genuine
        // collision, not a dialect branch -- inheritance must not create
        // a loophole that suppresses a real duplicate.
        let source = r#"<mapper namespace="x">
            <insert id="insertWidget" databaseId="oracle">
                <selectKey keyProperty="id" resultType="long">SELECT 1 FROM dual</selectKey>
                <selectKey keyProperty="id" resultType="long" databaseId="oracle">SELECT 2 FROM dual</selectKey>
                INSERT INTO widget (id) VALUES (#{id})
            </insert>
        </mapper>"#;
        let result = parse_str(source);
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DuplicateStatementId));
    }

    #[test]
    fn b36_duplicate_attribute_on_a_top_level_include_is_reported_once() {
        // Cold code review B36 (minor): a top-level <include> is visited by
        // both lift_includes (parse.rs, top-level-only) and flatten_body's
        // own descent (record_include, which sees every <include>,
        // including top-level ones) -- merge_includes already dedupes the
        // resulting IncludeRef entries by span, but each pass independently
        // re-scans the tag's own attributes, so an anomaly in that scan
        // (here: a duplicated refid attribute) was reported once per pass.
        let source = r#"<mapper namespace="n">
            <sql id="frag">x</sql>
            <select id="a">SELECT 1 <include refid="frag" refid="frag2"/></select>
        </mapper>"#;
        let result = parse_str(source);
        let dup_attr: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::DuplicateAttribute)
            .collect();
        assert_eq!(
            dup_attr.len(),
            1,
            "expected exactly one DuplicateAttribute, got {dup_attr:?}"
        );
    }

    #[test]
    fn b36_dedup_diagnostics_collapses_identical_code_and_span_keeping_first() {
        let span_a = ByteSpan { start: 5, end: 10 };
        let span_b = ByteSpan { start: 20, end: 25 };
        let mut diagnostics = vec![
            Diagnostic {
                code: DiagCode::DuplicateAttribute,
                span: Some(span_a),
                message: "first".to_string(),
            },
            Diagnostic {
                code: DiagCode::DuplicateAttribute,
                span: Some(span_a),
                message: "second (duplicate, should be dropped)".to_string(),
            },
            Diagnostic {
                code: DiagCode::DuplicateAttribute,
                span: Some(span_b),
                message: "different span, kept".to_string(),
            },
            Diagnostic {
                code: DiagCode::UnknownElement,
                span: Some(span_a),
                message: "different code, same span, kept".to_string(),
            },
        ];
        dedup_diagnostics(&mut diagnostics);
        assert_eq!(diagnostics.len(), 3);
        assert_eq!(diagnostics[0].message, "first");
        assert_eq!(diagnostics[1].message, "different span, kept");
        assert_eq!(diagnostics[2].message, "different code, same span, kept");
    }

    #[test]
    fn b36_dedup_diagnostics_never_collapses_none_span_entries() {
        // BranchLimitExceeded is span: None (whole-statement scope) and
        // legitimately recurs once per statement that independently
        // exceeds the cap -- deduping on code alone would silently drop
        // every occurrence past the first real one.
        let mut diagnostics = vec![
            Diagnostic {
                code: DiagCode::BranchLimitExceeded,
                span: None,
                message: "statement one".to_string(),
            },
            Diagnostic {
                code: DiagCode::BranchLimitExceeded,
                span: None,
                message: "statement two".to_string(),
            },
        ];
        dedup_diagnostics(&mut diagnostics);
        assert_eq!(diagnostics.len(), 2);
    }

    /// Test harness for [`capture_body`]: `source` must be a single element
    /// whose body is what's under test, e.g. `<select id="x">...</select>`.
    fn run_capture(source: &str) -> (Vec<BodySegment>, Vec<Diagnostic>, bool) {
        let mut reader = Reader::from_str(source);
        // A18: mirror every production caller's config (see parse_str).
        reader.config_mut().allow_dangling_amp = true;
        reader.read_event().expect("wrapper start tag");
        capture_body(source, &mut reader, 0)
    }

    fn text_decoded(segments: &[BodySegment]) -> Vec<&str> {
        segments
            .iter()
            .map(|s| match s {
                BodySegment::Text(t) => t.decoded.as_str(),
                BodySegment::DynamicTag { .. } => panic!("expected a text segment"),
            })
            .collect()
    }

    #[test]
    fn mm_08_cdata_with_comparison_operators() {
        let source = "<select id=\"x\"><![CDATA[ ROWNUM <= 10 ]]></select>";
        let (segments, diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        assert!(diagnostics.is_empty());
        assert_eq!(segments.len(), 1);
        let BodySegment::Text(seg) = &segments[0] else {
            panic!("expected a text segment")
        };
        assert!(seg.decoded.contains("<="));
        assert_eq!(seg.decoded, " ROWNUM <= 10 ");
        let ByteSpan { start, end } = seg.raw_span;
        assert_eq!(&source[start as usize..end as usize], seg.decoded);
    }

    #[test]
    fn mm_08_split_cdata_concatenates_into_consecutive_segments() {
        // Encodes the literal "]]>" split across two CDATA sections, the
        // standard XML escaping trick since CDATA can't contain "]]>"
        // directly.
        let source = "<select id=\"x\"><![CDATA[]]]]><![CDATA[>]]></select>";
        let (segments, diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        assert!(diagnostics.is_empty());
        assert_eq!(text_decoded(&segments), vec!["]]", ">"]);
        let joined: String = text_decoded(&segments).concat();
        assert_eq!(joined, "]]>");
    }

    #[test]
    fn mm_08_predefined_and_numeric_entities_decoded_raw_span_preserved() {
        // A10 (cold code review): as of quick-xml 0.41, each entity/
        // character reference tokenizes as its own `Event::GeneralRef`
        // rather than living inside one big `Event::Text` blob (see
        // parse.rs's `Ok(Event::GeneralRef(..))` arm) -- so this now
        // captures as several segments, not one. The invariants that
        // actually matter (every byte accounted for, contiguous spans,
        // decoded content correct, raw span slices back to the exact
        // original substring) are asserted directly instead of pinning
        // today's exact segment count.
        let source = "<select id=\"x\">a &lt;b&gt;&amp;&quot;&apos; &#64; &#x40;</select>";
        let (segments, diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        assert!(diagnostics.is_empty());
        assert!(
            segments.len() > 1,
            "expected multiple entity-split segments"
        );

        let mut decoded = String::new();
        let mut prev_end: Option<u32> = None;
        for seg in &segments {
            let BodySegment::Text(seg) = seg else {
                panic!("expected only text segments")
            };
            if let Some(prev_end) = prev_end {
                assert_eq!(
                    seg.raw_span.start, prev_end,
                    "segments must be contiguous, no gaps/overlaps"
                );
            }
            prev_end = Some(seg.raw_span.end);
            decoded.push_str(&seg.decoded);
        }
        assert_eq!(decoded, "a <b>&\"' @ @");

        let first = segments.first().unwrap();
        let BodySegment::Text(first) = first else {
            unreachable!()
        };
        let last = segments.last().unwrap();
        let BodySegment::Text(last) = last else {
            unreachable!()
        };
        assert_eq!(
            &source[first.raw_span.start as usize..last.raw_span.end as usize],
            "a &lt;b&gt;&amp;&quot;&apos; &#64; &#x40;"
        );
    }

    #[test]
    fn mm_08_undefined_entity_degrades_gracefully_with_diagnostic() {
        // A10 (cold code review): "a&nbsp;b" now tokenizes as three
        // segments (Text "a", GeneralRef "&nbsp;", Text "b") rather than
        // one -- see the comment on the test above. Concatenated decoded
        // content and diagnostic behavior are unchanged.
        let source = "<select id=\"x\">a&nbsp;b</select>";
        let (segments, diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        let decoded: String = segments
            .iter()
            .map(|seg| {
                let BodySegment::Text(seg) = seg else {
                    panic!("expected only text segments")
                };
                seg.decoded.as_str()
            })
            .collect();
        // Degrades to the raw (unresolved) text rather than dropping it.
        assert_eq!(decoded, "a&nbsp;b");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagCode::InvalidEntity);
        assert_eq!(
            diagnostics[0].span,
            Some(ByteSpan {
                start: source.find("&nbsp;").unwrap() as u32,
                end: (source.find("&nbsp;").unwrap() + "&nbsp;".len()) as u32,
            }),
            "diagnostic span should point at exactly the bad reference, not the whole text run"
        );
    }

    #[test]
    fn mm_08_undefined_entity_does_not_drop_the_statement() {
        let source = r#"<mapper namespace="x"><select id="a">a&nbsp;b</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::InvalidEntity));
    }

    // A18 (cold code review, BLOCKER): quick-xml 0.41's `allow_dangling_amp`
    // defaults to `false`, so a bare `&` used to make `read_event` return an
    // error that `capture_body`'s recovery path treated as unrecoverable
    // markup -- silently dropping everything up to the next `<` (SQL text
    // *and* any placeholder binding in it) and reporting `UnclosedTag`
    // instead of `InvalidEntity`. These pin the fix: the reader config
    // change plus the new diagnostic in the `Event::Text` arm.

    #[test]
    fn a18_bare_ampersand_is_kept_as_literal_text_with_diagnostic() {
        let source = "<select id=\"x\">a & b</select>";
        let (segments, diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        assert_eq!(text_decoded(&segments).concat(), "a & b");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagCode::InvalidEntity);
        let dangling_start = source.find('&').unwrap() as u32;
        let span = diagnostics[0].span.unwrap();
        assert_eq!(span.start, dangling_start);
        // B43 (cold code review): the span used to run from the `&` all
        // the way to wherever quick-xml next cut the Text event -- here,
        // to end of input, swallowing " b" (innocent SQL text) into the
        // "invalid entity" span. It must be exactly the one `&` byte.
        assert_eq!(
            span.end,
            dangling_start + 1,
            "dangling-amp InvalidEntity span must be exactly 1 byte (just the '&'), not the rest of the Text event"
        );
    }

    #[test]
    fn a18_doubled_bare_ampersand_produces_one_diagnostic_per_dangling_amp() {
        let source = "<select id=\"x\">a && b</select>";
        let (segments, diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        assert_eq!(text_decoded(&segments).concat(), "a && b");
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics
            .iter()
            .all(|d| d.code == DiagCode::InvalidEntity));
    }

    #[test]
    fn a18_unterminated_named_reference_without_semicolon_is_kept_literal() {
        // No trailing `;` after `amp` -- never a well-formed reference, so
        // it must not resolve to `&` the way `&amp;` would.
        let source = "<select id=\"x\">a &amp b</select>";
        let (segments, diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        assert_eq!(text_decoded(&segments).concat(), "a &amp b");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagCode::InvalidEntity);
    }

    #[test]
    fn a18_placeholder_after_bare_ampersand_survives_and_normalizes() {
        let source = r#"<mapper namespace="x"><select id="a">a & #{v}</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1, "statement must not be dropped");
        let stmt = &mapper.statements[0];
        let SqlText::Variants(variants) = &stmt.sql else {
            panic!("expected a single unconditional variant")
        };
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].text.text, "a & ?");
        assert_eq!(
            stmt.property_paths
                .iter()
                .map(|p| p.value.as_str())
                .collect::<Vec<_>>(),
            vec!["v"],
            "the placeholder after the dangling '&' must still bind"
        );
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::InvalidEntity));
    }

    #[test]
    fn a18_bare_ampersand_inside_if_still_diagnosed_and_content_preserved() {
        let source = r#"<mapper namespace="x"><select id="a"><if test="x != null">a & #{v}</if></select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let stmt = &mapper.statements[0];
        let SqlText::Variants(variants) = &stmt.sql else {
            panic!("expected variants (if without else)")
        };
        assert_eq!(variants.len(), 2, "not-taken + taken branch");
        assert!(variants.iter().any(|v| v.text.text.contains("a & ?")));
        assert_eq!(
            stmt.property_paths
                .iter()
                .map(|p| p.value.as_str())
                .collect::<Vec<_>>(),
            vec!["v"]
        );
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::InvalidEntity));
    }

    #[test]
    fn b44_dangling_amp_before_root_is_diagnosed() {
        // B44 (cold code review, major): A18's dangling-`&` diagnosis
        // lived only in capture_body (inside a statement body) -- a bare
        // `&` before the root element used to fall into parse_str's
        // top-level catch-all arm and vanish with zero diagnostics.
        let source = "&<mapper namespace=\"x\"></mapper>";
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Mybatis);
        assert!(result.mapper.is_some(), "the root element must still parse");
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|d| d.code == DiagCode::InvalidEntity)
                .count(),
            1,
            "expected exactly one InvalidEntity diagnostic, got {:?}",
            result.diagnostics
        );
        let diag = result
            .diagnostics
            .iter()
            .find(|d| d.code == DiagCode::InvalidEntity)
            .unwrap();
        let span = diag
            .span
            .expect("dangling amp diagnostic must carry a span");
        assert_eq!(span.start, 0);
        assert_eq!(span.end, 1, "span must be exactly the 1-byte '&' (B43)");
    }

    #[test]
    fn b44_dangling_amp_between_mapper_level_statements_is_diagnosed() {
        // B44: a bare `&` between sibling statements at mapper level
        // (not inside any statement's own body) used to fall into
        // build_mapper's catch-all arm and vanish with zero diagnostics.
        let source = r#"<mapper namespace="x"><select id="a">SELECT 1</select>& stray<select id="b">SELECT 2</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(
            mapper.statements.len(),
            2,
            "both statements must still be captured despite the stray '&' between them"
        );
        assert_eq!(mapper.statements[0].id.as_ref().unwrap().value, "a");
        assert_eq!(mapper.statements[1].id.as_ref().unwrap().value, "b");

        let amp_pos = source.find('&').unwrap() as u32;
        let amp_diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::InvalidEntity)
            .collect();
        assert_eq!(
            amp_diags.len(),
            1,
            "expected exactly one InvalidEntity diagnostic for the mapper-level '&', got {:?}",
            result.diagnostics
        );
        let span = amp_diags[0]
            .span
            .expect("dangling amp diagnostic must carry a span");
        assert_eq!(span.start, amp_pos);
        assert_eq!(
            span.end,
            amp_pos + 1,
            "span must be exactly the 1-byte '&' (B43)"
        );
    }

    #[test]
    fn b44_dangling_amp_inside_statement_body_is_not_double_diagnosed() {
        // B44: the mapper-level check added alongside capture_body's
        // existing one must not double-diagnose a dangling amp that's
        // actually inside a statement's own body -- that Text event is
        // consumed by capture_body itself and never reaches build_mapper's
        // loop at all, so there's naturally only one diagnosis, not a
        // B36-dedup-dependent one.
        let source = r#"<mapper namespace="x"><select id="a">a & b</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|d| d.code == DiagCode::InvalidEntity)
                .count(),
            1,
            "expected exactly one InvalidEntity diagnostic for the in-body '&', got {:?}",
            result.diagnostics
        );
    }

    // B46 (cold code review, moderate): B44 added `Event::Text` arms (dangling
    // '&') to parse_str's top-level loop and build_mapper's loop, but not
    // `Event::GeneralRef` arms -- so a well-formed but *unresolvable* named/
    // numeric reference (`&nbsp;`, `&#xD800;`) at those layers still vanished
    // with zero diagnostics, even though the identical reference inside a
    // statement body gets `InvalidEntity` from capture_body. These pin the
    // fix (`resolve_general_ref`, shared with capture_body's own arm).

    #[test]
    fn b46_unresolvable_named_entity_before_root_is_diagnosed() {
        let source = "&nbsp;<mapper namespace=\"x\"></mapper>";
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Mybatis);
        assert!(result.mapper.is_some(), "the root element must still parse");
        let diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::InvalidEntity)
            .collect();
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one InvalidEntity diagnostic, got {:?}",
            result.diagnostics
        );
        let span = diags[0].span.expect("must carry a span");
        assert_eq!(span.start, 0);
        assert_eq!(
            span.end,
            "&nbsp;".len() as u32,
            "span must cover the full reference, '&' through ';' inclusive"
        );
    }

    #[test]
    fn b46_unresolvable_named_entity_at_mapper_level_is_diagnosed() {
        let source = r#"<mapper namespace="x"><select id="a">SELECT 1</select>&nbsp;<select id="b">SELECT 2</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(
            mapper.statements.len(),
            2,
            "both statements must still be captured despite the stray reference between them"
        );

        let ref_pos = source.find("&nbsp;").unwrap() as u32;
        let diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::InvalidEntity)
            .collect();
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one InvalidEntity diagnostic, got {:?}",
            result.diagnostics
        );
        let span = diags[0].span.expect("must carry a span");
        assert_eq!(span.start, ref_pos);
        assert_eq!(span.end, ref_pos + "&nbsp;".len() as u32);
    }

    #[test]
    fn b46_invalid_numeric_reference_at_mapper_level_is_diagnosed() {
        // &#xD800; is an unpaired UTF-16 surrogate codepoint -- never a
        // valid Unicode scalar value, so it's unresolvable regardless of
        // being well-formed XML syntax.
        let source = r#"<mapper namespace="x">&#xD800;<select id="a">SELECT 1</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);

        let ref_pos = source.find("&#xD800;").unwrap() as u32;
        let diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::InvalidEntity)
            .collect();
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one InvalidEntity diagnostic, got {:?}",
            result.diagnostics
        );
        let span = diags[0].span.expect("must carry a span");
        assert_eq!(span.start, ref_pos);
        assert_eq!(span.end, ref_pos + "&#xD800;".len() as u32);
    }

    #[test]
    fn b46_unresolvable_named_entity_at_sqlmap_level_is_diagnosed() {
        // Same gap, iBatis dialect: `<sqlMap>` shares build_mapper with
        // `<mapper>`, so this must be covered identically.
        let source = r#"<sqlMap><statement id="a">SELECT 1</statement>&nbsp;<statement id="b">SELECT 2</statement></sqlMap>"#;
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Ibatis);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 2);

        let ref_pos = source.find("&nbsp;").unwrap() as u32;
        let diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::InvalidEntity)
            .collect();
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one InvalidEntity diagnostic, got {:?}",
            result.diagnostics
        );
        let span = diags[0].span.expect("must carry a span");
        assert_eq!(span.start, ref_pos);
        assert_eq!(span.end, ref_pos + "&nbsp;".len() as u32);
    }

    #[test]
    fn b46_resolvable_entity_at_mapper_level_gets_no_diagnostic() {
        // `&amp;` is a well-formed, resolvable reference -- must not be
        // flagged as InvalidEntity at mapper level, same as it wouldn't be
        // inside a statement body.
        let source = r#"<mapper namespace="x"><select id="a">SELECT 1</select>&amp;<select id="b">SELECT 2</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 2);
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.code == DiagCode::InvalidEntity),
            "a resolvable entity reference at mapper level must not be diagnosed, got {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn b46_unresolvable_entity_inside_statement_body_is_not_double_diagnosed() {
        // Same guard as B44's in-body test, but for GeneralRef: the
        // mapper-level check must not double-diagnose a reference that's
        // actually inside a statement's own body -- that GeneralRef event
        // is consumed by capture_body itself and never reaches
        // build_mapper's loop at all.
        let source = r#"<mapper namespace="x"><select id="a">a&nbsp;b</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|d| d.code == DiagCode::InvalidEntity)
                .count(),
            1,
            "expected exactly one InvalidEntity diagnostic for the in-body reference, got {:?}",
            result.diagnostics
        );
    }

    // B47 (cold code review, minor): parse_str returned from inside the
    // root-Start/Empty arm as soon as the root element completed, never
    // reading another event -- so `</mapper>&` or `</mapper> &nbsp;` were
    // silently accepted. These pin `scan_trailing_content`, which applies
    // only the dangling-'&'/unresolvable-entity diagnostics (B44/B46)
    // after the root closes; other trailing content stays ignored.

    #[test]
    fn b47_dangling_amp_immediately_after_root_close_is_diagnosed() {
        let source = "<mapper namespace=\"x\"></mapper>&";
        let result = parse_str(source);
        assert!(result.mapper.is_some(), "the root element must still parse");
        let amp_pos = source.rfind('&').unwrap() as u32;
        let diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::InvalidEntity)
            .collect();
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one InvalidEntity diagnostic, got {:?}",
            result.diagnostics
        );
        let span = diags[0].span.expect("must carry a span");
        // Byte-verified 1-byte span, per spec.
        assert_eq!(span.start, amp_pos);
        assert_eq!(
            span.end,
            amp_pos + 1,
            "dangling-amp span after root close must be exactly 1 byte"
        );
        assert_eq!(&source[span.start as usize..span.end as usize], "&");
    }

    #[test]
    fn b47_unresolvable_entity_after_root_close_is_diagnosed_with_full_span() {
        let source = "<mapper namespace=\"x\"></mapper> &nbsp;";
        let result = parse_str(source);
        assert!(result.mapper.is_some());
        let ref_pos = source.find("&nbsp;").unwrap() as u32;
        let diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::InvalidEntity)
            .collect();
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one InvalidEntity diagnostic, got {:?}",
            result.diagnostics
        );
        let span = diags[0].span.expect("must carry a span");
        assert_eq!(span.start, ref_pos);
        assert_eq!(span.end, ref_pos + "&nbsp;".len() as u32);
        assert_eq!(
            &source[span.start as usize..span.end as usize],
            "&nbsp;",
            "span must cover the full reference, '&' through ';' inclusive"
        );
    }

    #[test]
    fn b47_plain_trailing_text_after_root_close_has_no_diagnostic() {
        // Ordinary trailing whitespace/text -- not an anomaly at all,
        // must not be flagged.
        let source = "<mapper namespace=\"x\"></mapper>\n   \n";
        let result = parse_str(source);
        assert!(result.mapper.is_some());
        assert!(
            result.diagnostics.is_empty(),
            "plain trailing whitespace must not be diagnosed, got {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn b47_trailing_content_after_sqlmap_close_is_equally_covered() {
        let source = "<sqlMap></sqlMap> &nbsp;";
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Ibatis);
        assert!(result.mapper.is_some());
        let diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::InvalidEntity)
            .collect();
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one InvalidEntity diagnostic, got {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn b47_dangling_amp_after_self_closed_root_is_diagnosed() {
        // Self-closed root (Event::Empty) is a separate code path from
        // Event::Start/build_mapper -- must be covered too.
        let source = "<mapper namespace=\"x\"/>&";
        let result = parse_str(source);
        assert!(result.mapper.is_some());
        let amp_pos = source.rfind('&').unwrap() as u32;
        let diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::InvalidEntity)
            .collect();
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one InvalidEntity diagnostic, got {:?}",
            result.diagnostics
        );
        let span = diags[0].span.expect("must carry a span");
        assert_eq!(span.start, amp_pos);
        assert_eq!(span.end, amp_pos + 1);
    }

    #[test]
    fn mm_08_whitespace_preserved_verbatim() {
        let source = "<select id=\"x\">   SELECT 1   </select>";
        let (segments, _diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        assert_eq!(text_decoded(&segments), vec!["   SELECT 1   "]);
    }

    #[test]
    fn mm_08_mixed_text_and_cdata_segments_in_document_order() {
        let source = "<select id=\"x\">SELECT 1 <![CDATA[FROM t]]> WHERE 1=1</select>";
        let (segments, diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        assert!(diagnostics.is_empty());
        assert_eq!(
            text_decoded(&segments),
            vec!["SELECT 1 ", "FROM t", " WHERE 1=1"]
        );
    }

    #[test]
    fn mm_08_dynamic_tag_recorded_as_opaque_marker() {
        let source = "<select id=\"x\">SELECT 1 <if test=\"a\">AND a = #{a}</if> more</select>";
        let (segments, diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        assert!(diagnostics.is_empty());
        assert_eq!(segments.len(), 3);
        assert!(matches!(&segments[0], BodySegment::Text(t) if t.decoded == "SELECT 1 "));
        match &segments[1] {
            BodySegment::DynamicTag { name, span } => {
                assert_eq!(name, "if");
                let ByteSpan { start, end } = *span;
                assert_eq!(
                    &source[start as usize..end as usize],
                    "<if test=\"a\">AND a = #{a}</if>"
                );
            }
            _ => panic!("expected a dynamic-tag marker"),
        }
        assert!(matches!(&segments[2], BodySegment::Text(t) if t.decoded == " more"));
    }

    #[test]
    fn mm_04_sql_fragment_is_collected() {
        let source = r#"<mapper namespace="x"><sql id="widgetCols">id, name</sql></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.fragments.len(), 1);
        let fragment = &mapper.fragments[0];
        assert_eq!(fragment.id.value, "widgetCols");
        let ByteSpan { start, end } = fragment.id.span;
        assert_eq!(&source[start as usize..end as usize], fragment.id.value);
        assert!(mapper.statements.is_empty());
    }

    #[test]
    fn mm_04_empty_element_fragment_is_collected() {
        let source = r#"<mapper namespace="x"><sql id="widgetCols"/></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.fragments.len(), 1);
        assert_eq!(mapper.fragments[0].id.value, "widgetCols");
    }

    #[test]
    fn mm_04_missing_fragment_id_is_dropped_with_diagnostic() {
        let source = r#"<mapper namespace="x"><sql>id, name</sql></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert!(mapper.fragments.is_empty());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, DiagCode::MissingStatementId);
    }

    #[test]
    fn mm_04_duplicate_fragment_id_reuses_duplicate_statement_id_code() {
        let source = r#"<mapper namespace="x">
            <sql id="dup">a</sql>
            <sql id="dup">b</sql>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.fragments.len(), 2);
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|d| d.code == DiagCode::DuplicateStatementId)
                .count(),
            1
        );
    }

    #[test]
    fn mm_04_fragment_and_statement_sharing_id_is_not_a_duplicate() {
        // Fragment ids and statement ids are separate id spaces.
        let source = r#"<mapper namespace="x">
            <sql id="shared">id, name</sql>
            <select id="shared">SELECT id, name FROM widget</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.fragments.len(), 1);
        assert_eq!(mapper.statements.len(), 1);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DuplicateStatementId));
    }

    #[test]
    fn mm_04_fragment_body_with_nested_include_does_not_choke() {
        let source = r#"<mapper namespace="x">
            <sql id="withInclude">
                id, name
                <include refid="otherFragment"/>
            </sql>
            <sql id="afterInclude">more</sql>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let ids: Vec<_> = mapper
            .fragments
            .iter()
            .map(|f| f.id.value.clone())
            .collect();
        assert_eq!(ids, vec!["withInclude", "afterInclude"]);
        // MM-05 lifts the marker into an IncludeRef; "otherFragment" isn't
        // defined in this file, so it's also flagged as dangling.
        assert_eq!(mapper.fragments[0].includes.len(), 1);
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DanglingRefid));
    }

    #[test]
    fn mm_05_local_include_is_lifted_and_resolves() {
        let source = r#"<mapper namespace="x">
            <sql id="widgetCols">id, name</sql>
            <select id="selectWidget">SELECT <include refid="widgetCols"/> FROM widget</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements[0].includes.len(), 1);
        let include = &mapper.statements[0].includes[0];
        assert_eq!(include.value.raw, "widgetCols");
        assert_eq!(
            include.value.target,
            IncludeTarget::Local("widgetCols".to_string())
        );
        let ByteSpan { start, end } = include.span;
        assert_eq!(
            &source[start as usize..end as usize],
            "<include refid=\"widgetCols\"/>"
        );
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DanglingRefid));
    }

    #[test]
    fn mm_05_mybatis_qualified_refid_splits_on_last_dot() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT <include refid="com.example.other.Mapper.widgetCols"/></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let include = &mapper.statements[0].includes[0];
        assert_eq!(
            include.value.target,
            IncludeTarget::Qualified {
                ns: "com.example.other.Mapper".to_string(),
                id: "widgetCols".to_string(),
            }
        );
        assert_eq!(include.value.raw, "com.example.other.Mapper.widgetCols");
        // Qualified (cross-file) targets are never dangling-checked here.
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DanglingRefid));
    }

    #[test]
    fn mm_05_ibatis_dot_containing_refid_stays_local_not_qualified() {
        // iBatis has no cross-namespace include syntax — a dot in a refid
        // is the ordinary local-id convention, not a namespace separator.
        let source = r#"<sqlMap>
            <sql id="WidgetDAO.commonWhere">use_yn = 'Y'</sql>
            <select id="a">SELECT * <include refid="WidgetDAO.commonWhere"/></select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let include = &mapper.statements[0].includes[0];
        assert_eq!(
            include.value.target,
            IncludeTarget::Local("WidgetDAO.commonWhere".to_string())
        );
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DanglingRefid));
    }

    #[test]
    fn mm_05_dynamic_refid_is_unresolvable() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT <include refid="${dynamicRefId}"/></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let include = &mapper.statements[0].includes[0];
        assert_eq!(include.value.target, IncludeTarget::Dynamic);
        assert_eq!(include.value.raw, "${dynamicRefId}");
        // Unresolvable at parse time — never dangling-checked.
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DanglingRefid));
    }

    #[test]
    fn mm_05_self_reference_is_recorded_without_hanging() {
        let source = r#"<mapper namespace="x">
            <sql id="a">base <include refid="a"/></sql>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.fragments.len(), 1);
        assert_eq!(mapper.fragments[0].includes.len(), 1);
        assert_eq!(
            mapper.fragments[0].includes[0].value.target,
            IncludeTarget::Local("a".to_string())
        );
        // Self-reference to a fragment id that DOES exist (itself) — not
        // dangling. This crate never expands includes, so no loop risk.
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DanglingRefid));
    }

    #[test]
    fn mm_05_dangling_local_refid_is_reported() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT <include refid="doesNotExist"/></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements[0].includes.len(), 1);
        let dangling: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::DanglingRefid)
            .collect();
        assert_eq!(dangling.len(), 1);
    }

    #[test]
    fn b22_dangling_refid_is_suppressed_entirely_for_ibatis() {
        // Cold code review B22: iBatis <sql> fragments are a global
        // cross-file registry by design (any sqlMap can reference any
        // other sqlMap's fragment by short name) -- this crate only ever
        // sees one file, so the intra-file dangling check is ~all noise
        // for iBatis and must not fire at all, unlike MyBatis (see the
        // test above) where it stays on.
        let source = r#"<sqlMap>
            <select id="WidgetDAO.a">SELECT <include refid="doesNotExistElsewhere"/></select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements[0].includes.len(), 1);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DanglingRefid));
    }

    #[test]
    fn mm_05_missing_refid_attribute_reports_dangling_refid() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT <include/></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert!(mapper.statements[0].includes.is_empty());
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|d| d.code == DiagCode::DanglingRefid)
                .count(),
            1
        );
    }

    #[test]
    fn mm_07_statement_property_paths_populated_mybatis() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT * FROM widget WHERE id = #{id} AND name = ${nameCol}</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let paths: Vec<_> = mapper.statements[0]
            .property_paths
            .iter()
            .map(|p| p.value.as_str())
            .collect();
        assert_eq!(paths, vec!["id", "nameCol"]);
    }

    #[test]
    fn mm_07_statement_property_paths_populated_ibatis() {
        let source = r#"<sqlMap>
            <select id="a">SELECT * FROM widget WHERE id = #id# AND grp = $grpCd$</select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let paths: Vec<_> = mapper.statements[0]
            .property_paths
            .iter()
            .map(|p| p.value.as_str())
            .collect();
        assert_eq!(paths, vec!["id", "grpCd"]);
    }

    #[test]
    fn mm_07_statement_placeholder_inside_cdata_end_to_end() {
        let source = "<mapper namespace=\"x\"><select id=\"a\"><![CDATA[SELECT * FROM t WHERE id = #{id}]]></select></mapper>";
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements[0].property_paths.len(), 1);
        assert_eq!(mapper.statements[0].property_paths[0].value, "id");
    }

    #[test]
    fn mm_07_fragment_has_no_property_paths_field_but_diagnostic_surfaces() {
        // SqlFragment has no property_paths field (fragment paths are only
        // meaningful once MM-06 inlines the fragment into a statement), but
        // an unterminated placeholder inside a fragment body must still be
        // diagnosed rather than silently swallowed.
        let source = r#"<mapper namespace="x">
            <sql id="broken">WHERE id = #{id</sql>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.fragments.len(), 1);
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::UnterminatedPlaceholder));
    }

    fn variants(sql: &SqlText) -> &[SqlVariant] {
        match sql {
            SqlText::Variants(v) => v,
            SqlText::Union { .. } => panic!("expected Variants, got Union"),
            SqlText::Unrecognized => panic!("expected Variants, got Unrecognized"),
        }
    }

    #[test]
    fn mm_06_if_present_and_absent_yields_two_variants() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<if test="a != null"> AND a = #{a}</if></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 2);
        let texts: Vec<_> = vs.iter().map(|v| v.text.text.as_str()).collect();
        assert!(texts.contains(&"SELECT 1"));
        assert!(texts.contains(&"SELECT 1 AND a = ?"));
        let present = vs
            .iter()
            .find(|v| v.text.text == "SELECT 1 AND a = ?")
            .unwrap();
        assert_eq!(present.conditions, vec!["a != null".to_string()]);
        let absent = vs.iter().find(|v| v.text.text == "SELECT 1").unwrap();
        assert!(absent.conditions.is_empty());
        // property_paths recurse into <if> bodies now.
        assert_eq!(mapper.statements[0].property_paths[0].value, "a");
    }

    #[test]
    fn mm_06_choose_when_otherwise_branches() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<choose>
                <when test="a != null"> AND a = #{a}</when>
                <when test="b != null"> AND b = #{b}</when>
                <otherwise> AND active = 1</otherwise>
            </choose></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 3);
        let by_text: std::collections::HashMap<_, _> = vs
            .iter()
            .map(|v| (v.text.text.clone(), v.conditions.clone()))
            .collect();
        assert_eq!(
            by_text[&"SELECT 1 AND a = ?".to_string()],
            vec!["a != null".to_string()]
        );
        assert_eq!(
            by_text[&"SELECT 1 AND b = ?".to_string()],
            vec!["b != null".to_string()]
        );
        assert_eq!(
            by_text[&"SELECT 1 AND active = 1".to_string()],
            Vec::<String>::new()
        );
    }

    #[test]
    fn mm_06_choose_without_otherwise_has_implicit_empty_alternative() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<choose>
                <when test="a != null"> AND a = #{a}</when>
            </choose></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 2);
        let texts: Vec<_> = vs.iter().map(|v| v.text.text.as_str()).collect();
        assert!(texts.contains(&"SELECT 1"));
        assert!(texts.contains(&"SELECT 1 AND a = ?"));
    }

    #[test]
    fn choose_child_that_is_neither_when_nor_otherwise_is_diagnosed_not_silently_dropped() {
        // Cold code review B7: a <choose> child that's neither <when> nor
        // <otherwise> (including a stray <include>) used to vanish with
        // no diagnostic at all.
        let source = r#"<mapper namespace="x">
            <sql id="cols">a, b</sql>
            <select id="a">SELECT 1<choose>
                <when test="a != null"> AND a = #{a}</when>
                <include refid="cols"/>
                <otherwise> AND a IS NULL</otherwise>
            </choose></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        // The branch structure itself is unaffected: <include> contributes
        // no branch, same as before.
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 2);
        // But it's no longer silent.
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::UnknownElement
                && d.message.contains("<include>")
                && d.message.contains("<choose>")));
    }

    #[test]
    fn mm_06_cartesian_product_of_sibling_ifs() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<if test="a"> AND a</if><if test="b"> AND b</if></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 4); // 2 x 2
        let texts: std::collections::HashSet<_> = vs.iter().map(|v| v.text.text.clone()).collect();
        assert!(texts.contains("SELECT 1"));
        assert!(texts.contains("SELECT 1 AND a"));
        assert!(texts.contains("SELECT 1 AND b"));
        assert!(texts.contains("SELECT 1 AND a AND b"));
    }

    #[test]
    fn mm_06_nested_if_inside_unhandled_container_still_branches() {
        // Tags MM-06b/06c don't yet specialize (e.g. iBatis-style
        // <dynamic>) are still a transparent passthrough: recursed into so
        // a nested <if> still branches, contributing no wrapper text.
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<dynamic><if test="a != null"> AND a = #{a}</if></dynamic></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 2);
        let texts: std::collections::HashSet<_> = vs.iter().map(|v| v.text.text.clone()).collect();
        assert!(texts.contains("SELECT 1"));
        assert!(texts.contains("SELECT 1 AND a = ?"));
    }

    #[test]
    fn mm_06_include_marker_becomes_comment_token() {
        let source = r#"<mapper namespace="x">
            <sql id="cols">id, name</sql>
            <select id="a">SELECT <include refid="cols"/> FROM widget</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(
            vs[0].text.text,
            "SELECT /* batis:include(cols) */ FROM widget"
        );
    }

    #[test]
    fn include_refid_containing_star_slash_does_not_terminate_the_comment_early() {
        // Cold code review B15: a refid containing "*/" would otherwise
        // close the /* batis:include(...) */ comment early, corrupting
        // the rest of the rendered SQL text.
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT <include refid="a*/b"/> FROM widget</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(
            vs[0].text.text,
            "SELECT /* batis:include(a*_/b) */ FROM widget"
        );
        // The IncludeRef.raw itself is untouched -- only the rendered
        // comment token is sanitized.
        assert_eq!(mapper.statements[0].includes[0].value.raw, "a*/b");
    }

    // --- B16 (cold code review): a placeholder split across a text/CDATA
    // boundary used to be misread as an unterminated placeholder in the
    // first segment plus stray leftover text in the second, since MM-07
    // normalized each BodySegment::Text independently. End-to-end (real
    // CDATA, through the full capture_body -> flatten pipeline) versions
    // of the placeholder.rs unit tests.

    #[test]
    fn placeholder_split_by_real_cdata_boundary_normalizes_correctly() {
        let source = r#"<mapper namespace="x">
            <select id="a">WHERE id = #{i<![CDATA[d}]]></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].text.text, "WHERE id = ?");
        assert!(result
            .diagnostics
            .iter()
            .all(|d| d.code != DiagCode::UnterminatedPlaceholder));
        assert_eq!(mapper.statements[0].property_paths[0].value, "id");
    }

    #[test]
    fn placeholder_split_across_two_real_cdata_sections_normalizes_correctly() {
        // Three segments: plain text "#{i", CDATA "d", CDATA "}".
        let source = r#"<mapper namespace="x">
            <select id="a">WHERE id = #{i<![CDATA[d]]><![CDATA[}]]></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].text.text, "WHERE id = ?");
        assert!(result
            .diagnostics
            .iter()
            .all(|d| d.code != DiagCode::UnterminatedPlaceholder));
        assert_eq!(mapper.statements[0].property_paths[0].value, "id");
    }

    #[test]
    fn merge_of_non_straddling_segments_stays_byte_identical_to_per_segment_span_map() {
        // Pure-merge invariant: a CDATA boundary with NO placeholder
        // crossing it must still produce a span_map entry at that
        // junction (matching what processing each segment independently
        // would have produced) -- not just at replacement points.
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<![CDATA[ FROM widget]]> WHERE a = #{a}</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].text.text, "SELECT 1 FROM widget WHERE a = ?");
        // 3 segments (plain, CDATA, plain-with-placeholder) -> at least 3
        // span_map entries: initial + CDATA junction + post-replacement.
        assert!(vs[0].text.span_map.len() >= 3);
        assert!(vs[0].text.span_map.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn mm_06_fragment_sql_is_also_flattened() {
        let source = r#"<mapper namespace="x">
            <sql id="cond"><if test="a != null"> AND a = #{a}</if></sql>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.fragments[0].sql);
        assert_eq!(vs.len(), 2);
    }

    #[test]
    fn mm_06_span_map_strictly_increasing_in_assembled_variant() {
        let source = r#"<mapper namespace="x">
            <select id="a">WHERE a = #{a} AND b = #{b}<if test="c != null"> AND c = #{c}</if></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        for v in vs {
            assert!(v.text.span_map.windows(2).all(|w| w[0].0 < w[1].0));
            assert_eq!(v.text.span_map[0].0, 0);
        }
    }

    /// Builds a `<select>` body containing a `<choose>` with `when_count`
    /// `<when>` branches and no `<otherwise>` — exactly `when_count + 1`
    /// alternatives (see MM-06a's `<choose>` branch algebra).
    fn choose_with_whens(when_count: usize) -> String {
        let whens: String = (0..when_count)
            .map(|i| format!(r#"<when test="c{i}">t{i}</when>"#))
            .collect();
        format!(
            r#"<mapper namespace="x"><select id="a"><choose>{whens}</choose></select></mapper>"#
        )
    }

    #[test]
    fn mm_06_branch_limit_boundary_31_stays_variants() {
        let source = choose_with_whens(30); // 30 whens + implicit empty = 31
        let result = parse_str(&source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(variants(&mapper.statements[0].sql).len(), 31);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::BranchLimitExceeded));
    }

    #[test]
    fn mm_06_branch_limit_boundary_32_stays_variants() {
        let source = choose_with_whens(31); // 31 whens + implicit empty = 32
        let result = parse_str(&source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(variants(&mapper.statements[0].sql).len(), 32);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::BranchLimitExceeded));
    }

    #[test]
    fn mm_06_branch_limit_boundary_33_falls_back_to_union() {
        let source = choose_with_whens(32); // 32 whens + implicit empty = 33
        let result = parse_str(&source);
        let mapper = result.mapper.expect("mapper root");
        match &mapper.statements[0].sql {
            SqlText::Union { branch_count, .. } => assert_eq!(*branch_count, 33),
            SqlText::Variants(_) => panic!("expected Union at 33 branches"),
            SqlText::Unrecognized => panic!("expected Union, got Unrecognized"),
        }
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::BranchLimitExceeded));
    }

    #[test]
    fn mm_06_full_flattening_snapshot() {
        let source = r#"<mapper namespace="com.example.WidgetMapper">
            <sql id="baseCols">id, name</sql>
            <select id="selectWidget">
                SELECT <include refid="baseCols"/> FROM widget
                <where>
                    <if test="name != null">AND name = #{name}</if>
                    <choose>
                        <when test="active"> AND active = 1</when>
                        <otherwise> AND active = 0</otherwise>
                    </choose>
                </where>
            </select>
        </mapper>"#;
        let result = parse_str(source);
        insta::assert_json_snapshot!(result);
    }

    #[test]
    fn mm_06b_where_omitted_when_inner_is_empty() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<where><if test="a != null"> AND a = #{a}</if></where></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 2);
        let texts: std::collections::HashSet<_> = vs.iter().map(|v| v.text.text.clone()).collect();
        assert!(texts.contains("SELECT 1")); // absent branch: no WHERE at all
        assert!(texts.contains("SELECT 1WHERE a = ?"));
    }

    #[test]
    fn mm_06b_where_strips_one_leading_and_case_insensitive() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<where> and a = #{a} AND b = #{b}</where></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].text.text, "SELECT 1WHERE a = ? AND b = ?");
    }

    #[test]
    fn mm_06b_where_span_map_integrity_through_strip() {
        let source = r#"<mapper namespace="x">
            <select id="a"><where> AND a = #{a}</where></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        let v = &vs[0];
        assert_eq!(v.text.text, "WHERE a = ?");
        // Strictly increasing (model invariant) and every raw offset is a
        // valid position within the source.
        assert!(v.text.span_map.windows(2).all(|w| w[0].0 < w[1].0));
        for (_, raw) in &v.text.span_map {
            assert!((*raw as usize) <= source.len());
        }
        // The synthetic "WHERE " prefix's entry points at the <where>
        // tag's own span start (same convention as the include token).
        assert_eq!(v.text.span_map[0].0, 0);
    }

    // --- A1 (cold code review): leading_and_or_strip_len/
    // leading_override_strip_len/trailing_override_strip_len sliced &str
    // by byte length without a char-boundary check, panicking whenever a
    // multibyte character (CJK, emoji, ...) sat at the probed position.
    // Not mm_-prefixed: this is a cold-review fix, not a spec micro-feature.

    #[test]
    fn where_leading_and_or_strip_does_not_panic_on_leading_cjk() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<where>사용여부 = 'Y'</where></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "SELECT 1WHERE 사용여부 = 'Y'");
    }

    #[test]
    fn where_leading_and_or_strip_does_not_panic_on_leading_emoji() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<where>🎉 flag = 1</where></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "SELECT 1WHERE 🎉 flag = 1");
    }

    #[test]
    fn trim_prefix_overrides_leading_strip_does_not_panic_on_leading_cjk() {
        let source = r#"<mapper namespace="x">
            <select id="a"><trim prefix="WHERE" prefixOverrides="AND |OR ">사용여부 = 'Y'</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "WHERE 사용여부 = 'Y'");
    }

    #[test]
    fn trim_prefix_overrides_leading_strip_does_not_panic_on_leading_emoji() {
        let source = r#"<mapper namespace="x">
            <select id="a"><trim prefix="WHERE" prefixOverrides="AND |OR ">🎉 = 1</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "WHERE 🎉 = 1");
    }

    #[test]
    fn trim_suffix_overrides_trailing_strip_does_not_panic_on_trailing_cjk() {
        let source = r#"<mapper namespace="x">
            <select id="a"><trim suffixOverrides=",">a = 1사용여부</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "a = 1사용여부");
    }

    #[test]
    fn trim_suffix_overrides_trailing_strip_does_not_panic_on_trailing_emoji() {
        let source = r#"<mapper namespace="x">
            <select id="a"><trim suffixOverrides=",">a = 1🎉</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "a = 1🎉");
    }

    #[test]
    fn mm_06b_set_prepends_and_strips_trailing_comma() {
        let source = r#"<mapper namespace="x">
            <update id="a">UPDATE t <set>name = #{name}, age = #{age},</set></update>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].text.text, "UPDATE t SET name = ?, age = ?");
    }

    #[test]
    fn b17_set_strips_leading_comma_too() {
        // MyBatis 3.4.5+'s SetSqlNode uses prefixOverrides="," in addition
        // to suffixOverrides="," -- a leading comma (common when a
        // dynamic-tag chain conditionally omits the first assignment) must
        // be stripped, not just a trailing one.
        let source = r#"<mapper namespace="x">
            <update id="a">UPDATE t <set>, a = #{a}</set></update>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].text.text, "UPDATE t SET a = ?");
    }

    #[test]
    fn set_span_map_has_no_phantom_entry_at_stripped_comma_position() {
        // Cold code review B9: with_suffix_strip's span_map filter kept an
        // entry exactly at the truncated text's own end (off <= keep_len
        // instead of off < keep_len) -- a phantom entry describing a
        // segment with zero surviving characters (everything from that
        // offset onward was the just-stripped trailing comma).
        let source = r#"<mapper namespace="x">
            <update id="a">UPDATE t <set>x = #{a},</set></update>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        let v = &vs[0];
        assert_eq!(v.text.text, "UPDATE t SET x = ?");
        for (off, _) in &v.text.span_map {
            assert!(
                (*off as usize) < v.text.text.len(),
                "span_map entry at offset {off} is >= text length {} (phantom one-past-end entry)",
                v.text.text.len()
            );
        }
    }

    /// Shared assertion for the B26 reproductions below: no span_map entry
    /// may sit at or past the final text's own length.
    fn assert_no_phantom_one_past_end_entry(sql: &SqlString) {
        for (off, _) in &sql.span_map {
            assert!(
                (*off as usize) < sql.text.len(),
                "span_map entry at offset {off} is >= text length {} (phantom one-past-end entry)",
                sql.text.len()
            );
        }
    }

    #[test]
    fn b26_where_span_map_has_no_phantom_entry_when_strip_empties_the_body() {
        // Cold code review B26: with_prefix's split-point entry (pushed at
        // offset == prefix.len()) becomes a phantom one-past-end entry
        // when the leading strip consumes the *entire* body -- "AND " is
        // non-whitespace, so it doesn't hit expand_where's "contributes
        // nothing" shortcut, but the leading-AND/OR strip still eats all
        // of it, leaving `kept` empty and the final text exactly
        // "WHERE " (length == prefix.len()).
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<where>AND </where></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "SELECT 1WHERE ");
        assert_no_phantom_one_past_end_entry(&vs[0].text);
    }

    #[test]
    fn b26_set_span_map_has_no_phantom_entry_when_strip_empties_the_body() {
        // Same class: a <set> body that's nothing but a leading comma is
        // entirely consumed by the leading-comma strip (B17).
        let source = r#"<mapper namespace="x">
            <update id="a">UPDATE t <set>,</set></update>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "UPDATE t SET ");
        assert_no_phantom_one_past_end_entry(&vs[0].text);
    }

    #[test]
    fn b26_trim_span_map_has_no_phantom_entry_when_strip_empties_the_body() {
        // Same class: a <trim prefixOverrides> body that's nothing but
        // the override match itself is entirely consumed.
        let source = r#"<mapper namespace="x">
            <select id="a"><trim prefix="WHERE" prefixOverrides="AND ">AND </trim></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "WHERE ");
        assert_no_phantom_one_past_end_entry(&vs[0].text);
    }

    #[test]
    fn b20_set_span_map_has_no_phantom_entry_when_text_ends_on_a_placeholder() {
        // Cold code review B20: the normalize/assemble path itself (not
        // just with_suffix_strip, B9) can emit a span_map entry at exactly
        // `text.len()` -- a placeholder-with-options normalization
        // unconditionally pushes an entry right after the replacement, and
        // when that replacement is the last thing in the text (no
        // trailing comma to strip this time -- jdbcType option, not a
        // bare placeholder), the entry lands one byte past the end.
        let source = r#"<mapper namespace="x">
            <update id="a">UPDATE t <set>a = #{a,jdbcType=VARCHAR}</set></update>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        let v = &vs[0];
        assert_eq!(v.text.text, "UPDATE t SET a = ?");
        for (off, _) in &v.text.span_map {
            assert!(
                (*off as usize) < v.text.text.len(),
                "span_map entry at offset {off} is >= text length {} (phantom one-past-end entry)",
                v.text.text.len()
            );
        }
    }

    #[test]
    fn mm_06b_set_omitted_when_inner_is_empty() {
        let source = r#"<mapper namespace="x">
            <update id="a">UPDATE t <set><if test="a != null">name = #{name},</if></set></update>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        let texts: std::collections::HashSet<_> = vs.iter().map(|v| v.text.text.clone()).collect();
        assert!(texts.contains("UPDATE t "));
        assert!(texts.contains("UPDATE t SET name = ?"));
    }

    #[test]
    fn mm_06b_trim_prefix_suffix_and_overrides() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<trim prefix="WHERE (" suffix=")" prefixOverrides="AND |OR " suffixOverrides=",">AND a = #{a},</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].text.text, "SELECT 1WHERE ( a = ? )");
    }

    #[test]
    fn mm_06b_trim_contributes_nothing_when_inner_empty() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<trim prefix="WHERE"><if test="a != null">a = #{a}</if></trim></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        let texts: std::collections::HashSet<_> = vs.iter().map(|v| v.text.text.clone()).collect();
        assert!(texts.contains("SELECT 1"));
        assert!(texts.contains("SELECT 1WHERE a = ?"));
    }

    // A5 (cold code review, publication blocker): expand_trim fused
    // prefix/suffix directly onto the body (`WHEREwidget_name = ?`)
    // instead of inserting the separating space MyBatis's TrimSqlNode
    // always adds (`sql.insert(0, " ").insert(0, prefix)`,
    // `append(" ").append(suffix)`).

    #[test]
    fn a5_trim_prefix_inserts_space_when_override_strips_leading_and() {
        let source = r#"<mapper namespace="x">
            <select id="a"><trim prefix="WHERE" prefixOverrides="AND |OR ">AND widget_name = #{name}</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "WHERE widget_name = ?");
    }

    #[test]
    fn a5_trim_prefix_inserts_space_with_no_override_match() {
        let source = r#"<mapper namespace="x">
            <select id="a"><trim prefix="WHERE">a = 1</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "WHERE a = 1");
    }

    #[test]
    fn a5_trim_suffix_inserts_space_before_word_suffix() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<trim suffix="END">a = 1</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "SELECT 1a = 1 END");
    }

    #[test]
    fn a9_trim_override_overlap_does_not_panic() {
        // Cold code review A9 (publication blocker): lead_strip and
        // trail_strip are each computed independently against the *same*
        // original body text ("ABC"), and here both match their full
        // override length (2 bytes each) against a 3-byte body -- their
        // sum (4) exceeds the text length (3), which used to panic
        // (subtract-overflow in debug, OOB slice in release) inside
        // with_suffix_strip. Must return normally now.
        let source = r#"<mapper namespace="x">
            <select id="a"><trim prefixOverrides="AB" suffixOverrides="BC">ABC</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        assert!(result.mapper.is_some());
    }

    #[test]
    fn a9_trim_override_overlap_where_both_overrides_match_the_entire_body() {
        // Same class, more extreme: both overrides equal the whole body,
        // so a naive lead_strip + trail_strip would be double the text's
        // length.
        let source = r#"<mapper namespace="x">
            <select id="a"><trim prefixOverrides="ABC" suffixOverrides="ABC">ABC</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        assert!(result.mapper.is_some());
    }

    #[test]
    fn a9_trim_override_overlap_with_prefix_attribute_does_not_panic() {
        // Same overlap, but with a `prefix` attribute too (exercises the
        // with_prefix path with a non-empty prefix, not just strip_n > 0).
        let source = r#"<mapper namespace="x">
            <select id="a"><trim prefix="X" prefixOverrides="AB" suffixOverrides="BC">ABC</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        assert!(result.mapper.is_some());
    }

    #[test]
    fn b21_trim_leading_strip_over_entity_decoded_text_does_not_fabricate_offset() {
        // Cold code review B21, revised for A10: with_prefix's
        // strip-length extrapolation assumed 1 decoded byte == 1 raw
        // byte, which is false once entity decoding shrinks/expands byte
        // counts (`&#x41;` is 6 raw bytes for one decoded 'A'). B21
        // originally made this fall back to the whole segment's coarse
        // start, because quick-xml 0.37 tokenized the entire
        // "&#x41;ND widget_name = 1" run as one `Event::Text` blob with a
        // single unescape() call, giving no finer-grained information.
        //
        // quick-xml 0.41 (A10) tokenizes an entity/character reference as
        // its own `Event::GeneralRef`, separate from the surrounding
        // literal text (see parse.rs's `Ok(Event::GeneralRef(..))` arm).
        // So this source now captures as two segments -- the entity
        // itself (1 decoded byte from 6 raw bytes, non-verbatim) and
        // "ND widget_name = 1" (verbatim, raw byte-for-byte identical to
        // decoded) -- and with_prefix's honest byte-comparison check (see
        // its own comment) can correctly verify that stripping into the
        // *verbatim* second segment is safe, landing on the exact raw
        // offset right after the entity reference, not a fabricated one.
        // This is strictly more precise than the old coarse fallback, not
        // a regression: verified below by slicing the original bytes at
        // that offset and confirming it's exactly where "widget_name"
        // starts (i.e. exactly after "AND " in the conceptual decoded
        // text, `len("&#x41;ND ") == 9` raw bytes for 4 decoded ones).
        let source = r#"<mapper namespace="x">
            <select id="a"><trim prefixOverrides="AND |OR ">&#x41;ND widget_name = 1</trim></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs[0].text.text, "widget_name = 1");

        let entity_start = source.find("&#x41;").expect("entity in source") as u32;
        let expected_raw = entity_start + "&#x41;ND ".len() as u32;
        let (_, raw) = vs[0]
            .text
            .span_map
            .iter()
            .find(|(off, _)| *off == 0)
            .expect("span_map entry at offset 0");
        assert_eq!(
            *raw, expected_raw,
            "mapped offset must be the exact raw position right after the stripped \
             \"AND \" (verified honest via with_prefix's byte-comparison check), not \
             a fabricated offset"
        );
        // Slice the *original* bytes at that offset to prove it's a real,
        // meaningful position (the start of "widget_name"), not an
        // arbitrary byte count into unrelated content.
        assert_eq!(
            &source[*raw as usize..(*raw as usize + "widget_name".len())],
            "widget_name"
        );
    }

    // A7 (cold code review, major): DiagCode::IncludeAtWrapperBoundary is
    // emitted when an <include> is the first or last non-whitespace direct
    // child of a where/set/trim wrapper -- exactly the spot where MyBatis's
    // expand-before-evaluate order can bite a consumer who substitutes the
    // fragment text in afterward.

    #[test]
    fn a7_include_only_content_of_where_emits_boundary_diagnostic() {
        let source = r#"<mapper namespace="x">
            <sql id="frag">status = 'Y'</sql>
            <select id="a">SELECT 1<where><include refid="frag"/></where></select>
        </mapper>"#;
        let result = parse_str(source);
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::IncludeAtWrapperBoundary));
    }

    #[test]
    fn a7_include_first_in_set_emits_boundary_diagnostic() {
        let source = r#"<mapper namespace="x">
            <sql id="frag">name = #{name},</sql>
            <update id="a">UPDATE t <set><include refid="frag"/>age = #{age}</set></update>
        </mapper>"#;
        let result = parse_str(source);
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::IncludeAtWrapperBoundary));
    }

    #[test]
    fn a7_include_last_in_trim_emits_boundary_diagnostic() {
        let source = r#"<mapper namespace="x">
            <sql id="frag">status = 'Y'</sql>
            <select id="a"><trim prefix="WHERE">a = 1 AND <include refid="frag"/></trim></select>
        </mapper>"#;
        let result = parse_str(source);
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::IncludeAtWrapperBoundary));
    }

    #[test]
    fn a12_include_first_and_a_different_include_last_both_flagged() {
        // Cold code review A12 (major): the early `return` after the
        // first-position check used to fire unconditionally whenever the
        // first element was an include, silently skipping the
        // last-position check whenever the wrapper had a *different*
        // include as its last element too. Both must be flagged here --
        // two distinct diagnostics, one per include.
        let source = r#"<mapper namespace="x">
            <sql id="fragA">a = 1</sql>
            <sql id="fragB">b = 2</sql>
            <select id="x"><where><include refid="fragA"/> AND c = 3 AND <include refid="fragB"/></where></select>
        </mapper>"#;
        let result = parse_str(source);
        let boundary_diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::IncludeAtWrapperBoundary)
            .collect();
        assert_eq!(
            boundary_diags.len(),
            2,
            "both the first include and the distinct last include must be flagged"
        );
        let frag_a_span = source.find("<include refid=\"fragA\"").unwrap() as u32;
        let frag_b_span = source.find("<include refid=\"fragB\"").unwrap() as u32;
        assert!(boundary_diags
            .iter()
            .any(|d| d.span.unwrap().start == frag_a_span));
        assert!(boundary_diags
            .iter()
            .any(|d| d.span.unwrap().start == frag_b_span));
    }

    #[test]
    fn a12_single_include_as_only_content_is_flagged_exactly_once() {
        // Regression guard for the fix above: a single include (first ==
        // last, the same element) must still be reported only once, not
        // twice.
        let source = r#"<mapper namespace="x">
            <sql id="frag">status = 'Y'</sql>
            <select id="a">SELECT 1<where><include refid="frag"/></where></select>
        </mapper>"#;
        let result = parse_str(source);
        let boundary_diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::IncludeAtWrapperBoundary)
            .collect();
        assert_eq!(boundary_diags.len(), 1);
    }

    #[test]
    fn a12_back_to_back_includes_as_only_content_both_flagged() {
        // Two adjacent includes with no other content: distinct first and
        // last elements, both must be flagged.
        let source = r#"<mapper namespace="x">
            <sql id="fragA">a = 1</sql>
            <sql id="fragB">b = 2</sql>
            <select id="x"><where><include refid="fragA"/><include refid="fragB"/></where></select>
        </mapper>"#;
        let result = parse_str(source);
        let boundary_diags: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::IncludeAtWrapperBoundary)
            .collect();
        assert_eq!(boundary_diags.len(), 2);
    }

    #[test]
    fn a7_no_include_in_where_does_not_emit_boundary_diagnostic() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<where>status = 'Y'</where></select>
        </mapper>"#;
        let result = parse_str(source);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::IncludeAtWrapperBoundary));
    }

    #[test]
    fn a7_include_in_the_middle_of_where_does_not_emit_boundary_diagnostic() {
        let source = r#"<mapper namespace="x">
            <sql id="frag">status = 'Y'</sql>
            <select id="a">SELECT 1<where>a = 1 AND <include refid="frag"/> AND b = 2</where></select>
        </mapper>"#;
        let result = parse_str(source);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::IncludeAtWrapperBoundary));
    }

    #[test]
    fn a7_include_outside_any_wrapper_does_not_emit_boundary_diagnostic() {
        let source = r#"<mapper namespace="x">
            <sql id="frag">status = 'Y'</sql>
            <select id="a">SELECT 1 WHERE <include refid="frag"/></select>
        </mapper>"#;
        let result = parse_str(source);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::IncludeAtWrapperBoundary));
    }

    #[test]
    fn mm_06b_foreach_wraps_once_ignoring_separator_no_branch_factor() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<foreach item="i" collection="list" open="(" close=")" separator=",">#{i}</foreach></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1); // not a branch
        assert_eq!(vs[0].text.text, "SELECT 1(?)"); // separator not rendered
    }

    #[test]
    fn b19_foreach_without_open_keeps_inner_text_own_span_map_entry() {
        // Cold code review B19: with_prefix used to unconditionally
        // rewrite the first span_map entry to the wrapper tag's own span
        // start, even when prefix == "" and nothing was stripped (a
        // genuine no-op case) -- a <foreach> with no `open` attribute lost
        // its inner text's own (correct) first entry this way.
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<foreach item="i" collection="list">#{i}</foreach></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].text.text, "SELECT 1?");

        let placeholder_raw_offset = source.find("#{i}").expect("placeholder in source") as u32;
        let synthetic_offset = "SELECT 1".len() as u32; // where '?' begins
        let (_, raw) = vs[0]
            .text
            .span_map
            .iter()
            .find(|(off, _)| *off == synthetic_offset)
            .expect("span_map entry at the placeholder's synthetic offset");
        assert_eq!(
            *raw, placeholder_raw_offset,
            "mapped offset must point at the placeholder's own source position, not the <foreach> tag's start"
        );
    }

    #[test]
    fn mm_06b_bind_contributes_no_text_but_records_property_path() {
        let source = r#"<mapper namespace="x">
            <select id="a"><bind name="pattern" value="'%' + name + '%'"/>SELECT 1 WHERE name LIKE #{pattern}</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].text.text, "SELECT 1 WHERE name LIKE ?");
        let paths: Vec<_> = mapper.statements[0]
            .property_paths
            .iter()
            .map(|p| p.value.as_str())
            .collect();
        assert!(paths.contains(&"'%' + name + '%'"));
        assert!(paths.contains(&"pattern"));
    }

    #[test]
    fn mm_06b_nested_include_is_lifted_into_statement_includes() {
        // The known limitation flagged in MM-06a is closed here: an
        // <include> nested inside <if>/<where>/etc. must land in
        // Statement.includes, not just render correctly in the text.
        let source = r#"<mapper namespace="x">
            <sql id="cond">active = 1</sql>
            <select id="a">SELECT 1<where><if test="a != null"><include refid="cond"/></if></where></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements[0].includes.len(), 1);
        assert_eq!(mapper.statements[0].includes[0].value.raw, "cond");
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DanglingRefid));
    }

    #[test]
    fn mm_06b_nested_include_in_fragment_is_lifted_and_not_duplicated_with_top_level() {
        // A mix of a top-level include (seen by lift_includes) and a
        // nested one (only seen by flatten's descent) in the same body —
        // both must appear exactly once after merge_includes dedupes.
        let source = r#"<mapper namespace="x">
            <sql id="a">A</sql>
            <sql id="b">B</sql>
            <sql id="combined"><include refid="a"/><if test="x"><include refid="b"/></if></sql>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let combined = mapper
            .fragments
            .iter()
            .find(|f| f.id.value == "combined")
            .unwrap();
        let raws: Vec<_> = combined
            .includes
            .iter()
            .map(|i| i.value.raw.as_str())
            .collect();
        assert_eq!(raws, vec!["a", "b"]); // document order, no duplicates
    }

    #[test]
    fn mm_06c_is_not_empty_synthesizes_condition_and_branches() {
        let source = r#"<sqlMap>
            <select id="a">SELECT 1<isNotEmpty property="grpCd"> AND grp_cd = #grpCd#</isNotEmpty></select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 2);
        let present = vs.iter().find(|v| v.text.text.contains("grp_cd")).unwrap();
        assert_eq!(present.text.text, "SELECT 1 AND grp_cd = ?");
        assert_eq!(present.conditions, vec!["isNotEmpty(grpCd)".to_string()]);
        let absent = vs.iter().find(|v| !v.text.text.contains("grp_cd")).unwrap();
        assert_eq!(absent.text.text, "SELECT 1");
        assert!(absent.conditions.is_empty());
    }

    #[test]
    fn mm_06c_is_equal_condition_includes_compare_value() {
        let source = r#"<sqlMap>
            <select id="a">SELECT 1<isEqual property="status" compareValue="Y"> AND status = 'Y'</isEqual></select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        let present = vs.iter().find(|v| v.text.text.contains("status")).unwrap();
        assert_eq!(present.conditions, vec!["isEqual(status, 'Y')".to_string()]);
    }

    #[test]
    fn mm_06c_is_equal_condition_includes_compare_property() {
        let source = r#"<sqlMap>
            <select id="a">SELECT 1<isEqual property="a" compareProperty="b"> AND a = b</isEqual></select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        let present = vs.iter().find(|v| v.text.text.contains("a = b")).unwrap();
        assert_eq!(present.conditions, vec!["isEqual(a, b)".to_string()]);
    }

    #[test]
    fn mm_06c_unknown_is_tag_treated_as_generic_conditional_not_passthrough() {
        // A made-up "is*" tag we don't specifically recognize must still
        // branch (safer than silently flattening a conditional away).
        let source = r#"<sqlMap>
            <select id="a">SELECT 1<isMadeUpCondition property="x"> AND x = 1</isMadeUpCondition></select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 2);
        let present = vs.iter().find(|v| v.text.text.contains("x = 1")).unwrap();
        assert_eq!(present.conditions, vec!["isMadeUpCondition(x)".to_string()]);
    }

    #[test]
    fn mm_06c_conditional_prepend_joins_before_body_with_spaces() {
        let source = r#"<sqlMap>
            <select id="a">SELECT 1 WHERE 1=1<isNotEmpty property="a" prepend="AND">a = #a#</isNotEmpty></select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        let present = vs.iter().find(|v| v.text.text.contains("a = ?")).unwrap();
        assert_eq!(present.text.text, "SELECT 1 WHERE 1=1AND a = ?");
    }

    #[test]
    fn mm_06c_dynamic_suppresses_first_rendered_childs_prepend() {
        let source = r#"<sqlMap>
            <select id="a">SELECT 1<dynamic prepend="WHERE"><isNotEmpty property="a" prepend="AND">a = #a#</isNotEmpty> <isNotEmpty property="b" prepend="AND">b = #b#</isNotEmpty></dynamic></select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 4); // 2 x 2, dynamic itself isn't a branch

        let both = vs
            .iter()
            .find(|v| v.text.text.contains('a') && v.text.text.contains('b'));
        assert_eq!(both.unwrap().text.text, "SELECT 1WHERE a = ? AND b = ?");

        // Only one side present still carries the whitespace text segment
        // that sits between the two sibling tags in the source — this
        // isn't a bug, just a faithful rendering of that segment.
        let only_a = vs
            .iter()
            .find(|v| v.text.text.contains("a = ?") && !v.text.text.contains("b = ?"))
            .unwrap();
        assert_eq!(only_a.text.text, "SELECT 1WHERE a = ? ");

        let only_b = vs
            .iter()
            .find(|v| v.text.text.contains("b = ?") && !v.text.text.contains("a = ?"))
            .unwrap();
        assert_eq!(only_b.text.text, "SELECT 1WHERE  b = ?");

        let neither = vs
            .iter()
            .find(|v| !v.text.text.contains("a = ?") && !v.text.text.contains("b = ?"))
            .unwrap();
        assert_eq!(neither.text.text, "SELECT 1"); // dynamic contributes nothing when empty
    }

    #[test]
    fn mm_06c_iterate_wraps_once_ignores_conjunction_records_property() {
        let source = r#"<sqlMap>
            <select id="a">SELECT 1 WHERE id IN <iterate property="ids" open="(" close=")" conjunction=",">#ids[]#</iterate></select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1); // not a branch
        assert_eq!(vs[0].text.text, "SELECT 1 WHERE id IN (?)");
        let paths: Vec<_> = mapper.statements[0]
            .property_paths
            .iter()
            .map(|p| p.value.as_str())
            .collect();
        assert!(paths.contains(&"ids"));
        assert!(paths.contains(&"ids[]"));
    }

    #[test]
    fn mm_09_mybatis_parameter_and_result_type() {
        let source = r#"<mapper namespace="x">
            <select id="a" parameterType="long" resultType="com.example.Widget">SELECT 1</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let stmt = &mapper.statements[0];
        assert_eq!(stmt.param_class.as_ref().unwrap().value.raw, "long");
        assert_eq!(
            stmt.result_class.as_ref().unwrap().value.raw,
            "com.example.Widget"
        );
    }

    #[test]
    fn mm_09_ibatis_parameter_and_result_class() {
        let source = r#"<sqlMap>
            <select id="a" parameterClass="java.lang.String" resultClass="widget">SELECT 1</select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let stmt = &mapper.statements[0];
        assert_eq!(
            stmt.param_class.as_ref().unwrap().value.raw,
            "java.lang.String"
        );
        assert_eq!(stmt.result_class.as_ref().unwrap().value.raw, "widget");
    }

    #[test]
    fn mm_09_result_map_ref_attribute() {
        let source = r#"<mapper namespace="x">
            <select id="a" resultMap="widgetResult">SELECT 1</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(
            mapper.statements[0].result_map_ref.as_ref().unwrap().value,
            "widgetResult"
        );
    }

    #[test]
    fn mm_09_class_ref_generic_type_with_angle_brackets() {
        // Exercises </> inside a quoted attribute value through the
        // scan_attributes tokenizer (which consumes the quoted value as a
        // whole unit, so embedded '<'/'>' can't confuse it).
        let source = r#"<mapper namespace="x">
            <select id="a" resultType="java.util.List&lt;com.example.Widget&gt;">SELECT 1</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        // Entities in attribute values aren't decoded by scan_attributes
        // (same convention as id/namespace) — raw is kept exactly as it
        // appears in the source.
        assert_eq!(
            mapper.statements[0]
                .result_class
                .as_ref()
                .unwrap()
                .value
                .raw,
            "java.util.List&lt;com.example.Widget&gt;"
        );
    }

    #[test]
    fn mm_09_class_ref_array_type() {
        let source = r#"<mapper namespace="x">
            <select id="a" resultType="int[]">SELECT 1</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(
            mapper.statements[0]
                .result_class
                .as_ref()
                .unwrap()
                .value
                .raw,
            "int[]"
        );
    }

    #[test]
    fn mm_09_class_ref_alias_kept_raw_no_resolution() {
        let source = r#"<mapper namespace="x">
            <select id="a" resultType="widget">SELECT 1</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(
            mapper.statements[0]
                .result_class
                .as_ref()
                .unwrap()
                .value
                .raw,
            "widget"
        );
    }

    #[test]
    fn mm_10_resultmap_id_and_result_mappings() {
        let source = r#"<mapper namespace="x">
            <resultMap id="widgetResult" type="com.example.Widget">
                <id column="widget_id" property="id"/>
                <result column="widget_name" property="name"/>
            </resultMap>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.result_maps.len(), 1);
        let rm = &mapper.result_maps[0];
        assert_eq!(rm.id.value, "widgetResult");
        assert_eq!(
            rm.type_ref.as_ref().unwrap().value.raw,
            "com.example.Widget"
        );
        assert_eq!(rm.mappings.len(), 2);
        assert_eq!(rm.mappings[0].column.as_deref(), Some("widget_id"));
        assert_eq!(rm.mappings[0].property.as_deref(), Some("id"));
        assert_eq!(rm.mappings[1].column.as_deref(), Some("widget_name"));
        assert_eq!(rm.mappings[1].property.as_deref(), Some("name"));
    }

    #[test]
    fn mm_10_resultmap_extends() {
        let source = r#"<mapper namespace="x">
            <resultMap id="base" type="com.example.Base"><id column="id" property="id"/></resultMap>
            <resultMap id="derived" type="com.example.Derived" extends="base">
                <result column="extra" property="extra"/>
            </resultMap>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let derived = mapper
            .result_maps
            .iter()
            .find(|r| r.id.value == "derived")
            .unwrap();
        assert_eq!(derived.extends.as_ref().unwrap().value, "base");
        assert_eq!(derived.mappings.len(), 1);
    }

    #[test]
    fn mm_10_resultmap_nested_association_and_collection_flattened() {
        let source = r#"<mapper namespace="x">
            <resultMap id="orderResult" type="com.example.Order">
                <id column="order_id" property="id"/>
                <association property="widget" javaType="com.example.Widget">
                    <result column="widget_name" property="name"/>
                </association>
                <collection property="items" ofType="com.example.Item">
                    <result column="item_id" property="id"/>
                </collection>
            </resultMap>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let rm = &mapper.result_maps[0];
        let columns: Vec<_> = rm
            .mappings
            .iter()
            .map(|m| m.column.as_deref().unwrap())
            .collect();
        assert_eq!(columns, vec!["order_id", "widget_name", "item_id"]);
    }

    #[test]
    fn mm_10_resultmap_discriminator_case_flattened() {
        let source = r#"<mapper namespace="x">
            <resultMap id="widgetResult" type="com.example.Widget">
                <id column="widget_id" property="id"/>
                <discriminator column="widget_type" javaType="string">
                    <case value="A"><result column="a_field" property="aField"/></case>
                    <case value="B"><result column="b_field" property="bField"/></case>
                </discriminator>
            </resultMap>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let rm = &mapper.result_maps[0];
        let columns: Vec<_> = rm
            .mappings
            .iter()
            .map(|m| m.column.as_deref().unwrap())
            .collect();
        assert_eq!(columns, vec!["widget_id", "a_field", "b_field"]);
    }

    #[test]
    fn mm_10_resultmap_duplicate_id_diagnostic() {
        let source = r#"<mapper namespace="x">
            <resultMap id="dup" type="com.example.A"><id column="a" property="a"/></resultMap>
            <resultMap id="dup" type="com.example.B"><id column="b" property="b"/></resultMap>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.result_maps.len(), 2);
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|d| d.code == DiagCode::DuplicateStatementId)
                .count(),
            1
        );
    }

    #[test]
    fn mm_10_resultmap_id_space_separate_from_statement_and_fragment_ids() {
        let source = r#"<mapper namespace="x">
            <resultMap id="shared" type="com.example.A"><id column="a" property="a"/></resultMap>
            <sql id="shared">a</sql>
            <select id="shared">SELECT 1</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.result_maps.len(), 1);
        assert_eq!(mapper.fragments.len(), 1);
        assert_eq!(mapper.statements.len(), 1);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::DuplicateStatementId));
    }

    #[test]
    fn mm_10_resultmap_missing_id_dropped_with_diagnostic() {
        let source = r#"<mapper namespace="x">
            <resultMap type="com.example.A"><id column="a" property="a"/></resultMap>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert!(mapper.result_maps.is_empty());
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::MissingStatementId));
    }

    #[test]
    fn mm_10_resultmap_self_closing_has_empty_mappings() {
        let source = r#"<mapper namespace="x">
            <resultMap id="empty" type="com.example.A"/>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.result_maps.len(), 1);
        assert!(mapper.result_maps[0].mappings.is_empty());
    }

    // --- B10 (cold code review): a self-closed element (<select/>) used
    // to get sql: SqlText::Variants(vec![]) (zero variants) as its final
    // value -- never overwritten by flattening, since self-closed
    // elements never go through capture_body/flatten_body at all. An
    // empty-bodied element (<select></select>) *does* flatten (an empty
    // segment list), producing one variant with empty text. Two different
    // shapes for what should be the same "no body" case.

    #[test]
    fn self_closed_statement_and_empty_bodied_statement_have_identical_sql_shape() {
        let self_closed = r#"<mapper namespace="x"><select id="a"/></mapper>"#;
        let empty_bodied = r#"<mapper namespace="x"><select id="a"></select></mapper>"#;
        let a = parse_str(self_closed).mapper.expect("mapper root");
        let b = parse_str(empty_bodied).mapper.expect("mapper root");
        assert_eq!(a.statements[0].sql, b.statements[0].sql);
        assert_eq!(
            a.statements[0].sql,
            SqlText::Variants(vec![SqlVariant {
                text: SqlString {
                    text: String::new(),
                    span_map: Vec::new(),
                },
                conditions: Vec::new(),
            }])
        );
    }

    #[test]
    fn self_closed_fragment_and_empty_bodied_fragment_have_identical_sql_shape() {
        let self_closed = r#"<mapper namespace="x"><sql id="a"/></mapper>"#;
        let empty_bodied = r#"<mapper namespace="x"><sql id="a"></sql></mapper>"#;
        let a = parse_str(self_closed).mapper.expect("mapper root");
        let b = parse_str(empty_bodied).mapper.expect("mapper root");
        assert_eq!(a.fragments[0].sql, b.fragments[0].sql);
    }

    #[test]
    fn mm_13_orphan_closing_tag_ignored_and_parsing_continues() {
        // Recovery rule 2: an orphan/mismatched closing tag doesn't abort
        // parsing — both statements (before AND after the bad tag) must
        // survive, with a diagnostic for the malformed structure.
        let source = r#"<mapper namespace="x"><select id="a">SELECT 1</select></wrongclose><select id="b">SELECT 2</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let ids: Vec<_> = mapper
            .statements
            .iter()
            .map(|s| s.id.as_ref().unwrap().value.clone())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::UnclosedTag));
    }

    #[test]
    fn mm_13_bare_valueless_attribute_skipped_not_fatal_to_scan() {
        // The deferred MM-02 item: a bare valueless attribute used to stop
        // the whole attribute scan, losing every attribute after it.
        let source = r#"<mapper foo namespace="x"></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.namespace.as_ref().unwrap().value, "x");
    }

    // --- B6 (cold code review): a bare valueless attribute immediately
    // followed by the tag's own `>` used to make scan_attributes' name-scan
    // loop read straight through that `>` into the body text, fabricating
    // an attribute from SQL that was never inside the tag at all.

    #[test]
    fn scan_attributes_stops_at_tag_close_even_with_trailing_bare_attribute() {
        let source = r#"<if foo>x AND test = "1"</if>"#;
        // Deliberately the FULL element span (opening tag through closing
        // tag), matching how flatten.rs/parse.rs actually call
        // scan_attributes for a DynamicTag's span -- a `tag_end` bounded
        // to just the opening tag's own `>` would mask the bug (the outer
        // `i < tag_end` check would stop the scan either way).
        let tag_end = source.len();
        let attrs = scan_attributes(source.as_bytes(), 0, tag_end);
        assert!(
            attrs.is_empty(),
            "no real attributes in <if foo>, but scan found {}",
            attrs.len()
        );
    }

    #[test]
    fn bare_attribute_before_tag_close_does_not_fabricate_condition_from_body() {
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1<if foo>x AND test = "1"</if></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 2);
        let present = vs
            .iter()
            .find(|v| v.text.text.contains("AND test"))
            .expect("a present-branch variant containing the body text exists");
        // The <if>'s own `test` attribute is genuinely absent (`foo` is a
        // bare, unrelated attribute) -- its condition must be empty, never
        // "1" fabricated from the body's `test = "1"` text.
        assert_eq!(present.conditions, vec!["".to_string()]);
    }

    #[test]
    fn mm_13_oversize_input_yields_oversize_diagnostic() {
        let huge = "x".repeat(OVERSIZE_LIMIT + 1);
        let source = format!("<mapper namespace=\"x\"><select id=\"a\">{huge}</select></mapper>");
        let result = parse_str(&source);
        assert!(result.mapper.is_none());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, DiagCode::OversizeInput);
    }

    #[test]
    fn mm_13_under_cap_input_parses_normally() {
        let source = r#"<mapper namespace="x"><select id="a">SELECT 1</select></mapper>"#;
        assert!(source.len() < OVERSIZE_LIMIT);
        let result = parse_str(source);
        assert!(result.mapper.is_some());
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::OversizeInput));
    }

    #[test]
    fn mm_13_malformed_bang_markup_does_not_panic_and_diagnoses() {
        // Recovery rule 4: quick-xml's own tokenizer sometimes only
        // manages to resynchronize as far as EOF for certain malformed
        // constructs (verified: invalid "<!...>" markup) — the hard
        // invariant is no panic and no silent failure, not necessarily
        // full recovery of everything after it.
        let source =
            r#"<mapper namespace="x"><select id="a">before<!weird>after</select></mapper>"#;
        let result = parse_str(source);
        assert!(!result.diagnostics.is_empty());
        // Whatever was parsed before the malformed markup is preserved.
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
    }

    #[test]
    fn mm_11_ibatis_resultmap_class_attribute_read_as_type_ref() {
        // iBatis <resultMap> uses class= where MyBatis uses type=.
        let source = r#"<sqlMap>
            <resultMap id="widgetResult" class="widget">
                <result column="widget_id" property="id"/>
            </resultMap>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.result_maps.len(), 1);
        assert_eq!(
            mapper.result_maps[0].type_ref.as_ref().unwrap().value.raw,
            "widget"
        );
    }

    #[test]
    fn mm_11_mybatis_resultmap_type_attribute_still_works() {
        // Locks that MM-11's class= addition didn't regress MyBatis's
        // type= (MM-10's original behavior).
        let source = r#"<mapper namespace="x">
            <resultMap id="widgetResult" type="com.example.Widget">
                <result column="widget_id" property="id"/>
            </resultMap>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(
            mapper.result_maps[0].type_ref.as_ref().unwrap().value.raw,
            "com.example.Widget"
        );
    }

    #[test]
    fn mm_11_parameter_map_has_no_model_field_but_does_not_break_parsing() {
        // Documented limitation: <parameterMap> isn't captured anywhere
        // (the model is final, no field for it) — it must not prevent
        // sibling statements from parsing normally.
        let source = r#"<sqlMap>
            <parameterMap id="widgetParam" class="widget">
                <parameter property="id"/>
            </parameterMap>
            <select id="a" parameterMap="widgetParam">SELECT 1</select>
        </sqlMap>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        assert_eq!(mapper.statements[0].id.as_ref().unwrap().value, "a");
    }

    #[test]
    fn a14_known_ignorable_cache_element_is_not_diagnosed() {
        // Cold code review A14: <cache> (MyBatis, no model field -- same
        // class of deliberate gap as <parameterMap>) must stay silent, not
        // get an UnknownElement diagnostic.
        let source = r#"<mapper namespace="x">
            <cache eviction="LRU"/>
            <select id="a">SELECT 1</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::UnknownElement));
    }

    #[test]
    fn a14_statement_level_typo_is_diagnosed_and_dropped() {
        // Cold code review A14 (major): a typo'd statement tag like
        // <slect> (for <select>) used to drop the whole statement with
        // zero diagnostics -- indistinguishable from a deliberately
        // out-of-scope element like <cache>. Must now get an
        // UnknownElement diagnostic (the statement itself is still
        // dropped -- there's no model slot to recover it into).
        let source = r#"<mapper namespace="x">
            <slect id="typo">SELECT 1</slect>
            <select id="real">SELECT 2</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        assert_eq!(mapper.statements[0].id.as_ref().unwrap().value, "real");
        let unknown: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::UnknownElement)
            .collect();
        assert_eq!(unknown.len(), 1);
        assert!(unknown[0].message.contains("slect"));
    }

    #[test]
    fn a14_self_closed_statement_level_typo_is_diagnosed() {
        // Same as a14_statement_level_typo_is_diagnosed_and_dropped but
        // for the Event::Empty (self-closed) parse path specifically --
        // a separate code path from the non-empty Event::Start one.
        let source = r#"<mapper namespace="x">
            <slect id="typo"/>
            <select id="real">SELECT 1</select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        let unknown: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::UnknownElement)
            .collect();
        assert_eq!(unknown.len(), 1);
        assert!(unknown[0].message.contains("slect"));
    }

    #[test]
    fn a14_dynamic_position_typo_is_diagnosed_but_still_folded_transparently() {
        // Cold code review A14 (major): a typo'd dynamic tag like <iff>
        // (for <if>) used to fold its content in unconditionally with no
        // diagnostic at all -- must now get an UnknownElement diagnostic,
        // while *keeping* the transparent-fold recovery (the body text is
        // still present, not dropped, since that's the safer degrade for
        // a wrapper-shaped typo).
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1 <iff test="x != null">AND x = 1</iff></select>
        </mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let vs = variants(&mapper.statements[0].sql);
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].text.text, "SELECT 1 AND x = 1");
        let unknown: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::UnknownElement)
            .collect();
        assert_eq!(unknown.len(), 1);
        assert!(unknown[0].message.contains("iff"));
    }

    #[test]
    fn b38_unknown_element_s_descendants_do_not_each_get_their_own_diagnostic() {
        // Cold code review B38 (minor): an unknown element's content is
        // still walked by the same recursive descent that flagged it
        // (that's the deliberate transparent-fold recovery), so its own
        // children hit the same catch-all too -- a <resultMap> misplaced
        // inside a statement used to flag itself AND its <id>/<result>
        // children, one authoring mistake producing three diagnostics.
        // Only the outermost unknown element should be flagged.
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1
                <resultMap id="rm" type="Widget">
                    <id column="id" property="id"/>
                    <result column="name" property="name"/>
                </resultMap>
            </select>
        </mapper>"#;
        let result = parse_str(source);
        let unknown: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::UnknownElement)
            .collect();
        assert_eq!(
            unknown.len(),
            1,
            "only the outermost unknown element should be flagged, got {unknown:?}"
        );
        assert!(unknown[0].message.contains("resultMap"));
    }

    #[test]
    fn b38_sibling_unknown_elements_are_each_flagged_independently() {
        // The suppression must be scoped to one unknown element's own
        // subtree -- it must not leak into a later, unrelated sibling.
        let source = r#"<mapper namespace="x">
            <select id="a">SELECT 1
                <bogusOne>a</bogusOne>
                <bogusTwo>b</bogusTwo>
            </select>
        </mapper>"#;
        let result = parse_str(source);
        let unknown: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagCode::UnknownElement)
            .collect();
        assert_eq!(unknown.len(), 2, "each sibling gets its own: {unknown:?}");
    }

    // --- span field (Statement/SqlFragment/ResultMap): opening-tag start
    // -> subtree end. Promoted from the CodeGraph swap experiment (friction
    // #3); no MM number was assigned in the spec, so these aren't mm_-
    // prefixed like the rest of this module.

    #[test]
    fn span_covers_statement_opening_tag_through_subtree_end() {
        let source = r#"<mapper namespace="x"><select id="a">SELECT 1</select></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let expected_start = source.find("<select").unwrap() as u32;
        let expected_end = source.find("</select>").unwrap() as u32 + "</select>".len() as u32;
        assert_eq!(
            mapper.statements[0].span,
            ByteSpan {
                start: expected_start,
                end: expected_end
            }
        );
    }

    #[test]
    fn span_covers_self_closed_statement_equals_own_tag() {
        let source = r#"<mapper namespace="x"><select id="a"/></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let expected_start = source.find("<select").unwrap() as u32;
        let expected_end = source.find("/>").unwrap() as u32 + "/>".len() as u32;
        assert_eq!(
            mapper.statements[0].span,
            ByteSpan {
                start: expected_start,
                end: expected_end
            }
        );
    }

    #[test]
    fn span_covers_sql_fragment_opening_tag_through_subtree_end() {
        let source = r#"<mapper namespace="x"><sql id="cols">a, b</sql></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let expected_start = source.find("<sql").unwrap() as u32;
        let expected_end = source.find("</sql>").unwrap() as u32 + "</sql>".len() as u32;
        assert_eq!(
            mapper.fragments[0].span,
            ByteSpan {
                start: expected_start,
                end: expected_end
            }
        );
    }

    #[test]
    fn span_covers_result_map_opening_tag_through_subtree_end() {
        let source = r#"<mapper namespace="x"><resultMap id="rm" type="Widget"><result column="a" property="a"/></resultMap></mapper>"#;
        let result = parse_str(source);
        let mapper = result.mapper.expect("mapper root");
        let expected_start = source.find("<resultMap").unwrap() as u32;
        let expected_end =
            source.find("</resultMap>").unwrap() as u32 + "</resultMap>".len() as u32;
        assert_eq!(
            mapper.result_maps[0].span,
            ByteSpan {
                start: expected_start,
                end: expected_end
            }
        );
    }

    // --- A2 (cold code review): unbounded recursion in flatten_segments/
    // expand_*/union_walk (dynamic-tag flattening) and collect_mappings
    // (resultMap association/discriminator) could stack-overflow (abort,
    // uncatchable) on pathologically deep nesting -- far shallower under
    // wasm's 1MB stack. Both families now stop descending at
    // DEPTH_LIMIT=256 and diagnose instead of continuing.

    #[test]
    fn deeply_nested_if_tags_in_a_statement_returns_normally_with_diagnostic() {
        let mut body = String::from("x = 1");
        for _ in 0..3000 {
            body = format!(r#"<if test="a">{body}</if>"#);
        }
        let source =
            format!(r#"<mapper namespace="x"><select id="s">SELECT 1{body}</select></mapper>"#);
        let result = parse_str(&source); // must return normally, not abort
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::NestingLimitExceeded));
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.statements.len(), 1);
        assert_eq!(mapper.statements[0].id.as_ref().unwrap().value, "s");
    }

    #[test]
    fn deeply_nested_association_in_a_result_map_returns_normally_with_diagnostic() {
        let mut body = String::from(r#"<result column="c" property="p"/>"#);
        for _ in 0..3000 {
            body = format!(r#"<association property="a">{body}</association>"#);
        }
        let source = format!(
            r#"<mapper namespace="x"><resultMap id="rm" type="T">{body}</resultMap></mapper>"#
        );
        let result = parse_str(&source); // must return normally, not abort
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.code == DiagCode::NestingLimitExceeded));
        let mapper = result.mapper.expect("mapper root");
        assert_eq!(mapper.result_maps.len(), 1);
        assert_eq!(mapper.result_maps[0].id.value, "rm");
    }
}

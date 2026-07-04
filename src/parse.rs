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

/// MM-01: identifies the root element and derives the dialect from its
/// name (`<mapper>` → MyBatis, `<sqlMap>` → iBatis).
pub(crate) fn parse_str(source: &str) -> ParseResult {
    let mut reader = Reader::from_str(source);

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
                        let (mapper, diagnostics) = build_mapper(
                            source,
                            &mut reader,
                            start as usize,
                            end as usize,
                            dialect,
                        );
                        ParseResult {
                            dialect,
                            mapper: Some(mapper),
                            diagnostics,
                        }
                    }
                    None => ParseResult {
                        dialect: Dialect::Unknown,
                        mapper: None,
                        diagnostics: vec![Diagnostic {
                            code: DiagCode::UnknownElement,
                            span: Some(ByteSpan {
                                start: start as u32,
                                end: end as u32,
                            }),
                            message: format!(
                                "root element <{}> is not a mapper/sqlMap",
                                String::from_utf8_lossy(name.as_ref())
                            ),
                        }],
                    },
                };
            }
            Ok(Event::Empty(tag)) => {
                let end = reader.buffer_position();
                let name = tag.local_name();
                let name = name.as_ref();
                return match name {
                    b"mapper" => {
                        let (mapper, diagnostics) =
                            mapper_with_namespace(source, start as usize, end as usize);
                        ParseResult {
                            dialect: Dialect::Mybatis,
                            mapper: Some(mapper),
                            diagnostics,
                        }
                    }
                    b"sqlMap" => {
                        let (mapper, diagnostics) =
                            mapper_with_namespace(source, start as usize, end as usize);
                        ParseResult {
                            dialect: Dialect::Ibatis,
                            mapper: Some(mapper),
                            diagnostics,
                        }
                    }
                    other => ParseResult {
                        dialect: Dialect::Unknown,
                        mapper: None,
                        diagnostics: vec![Diagnostic {
                            code: DiagCode::UnknownElement,
                            span: Some(ByteSpan {
                                start: start as u32,
                                end: end as u32,
                            }),
                            message: format!(
                                "root element <{}> is not a mapper/sqlMap",
                                String::from_utf8_lossy(other)
                            ),
                        }],
                    },
                };
            }
            Ok(Event::Eof) => {
                return ParseResult {
                    dialect: Dialect::Unknown,
                    mapper: None,
                    diagnostics: vec![Diagnostic {
                        code: DiagCode::UnknownElement,
                        span: None,
                        message: "no root element found".to_string(),
                    }],
                };
            }
            Err(err) => {
                let pos = reader.error_position();
                return ParseResult {
                    dialect: Dialect::Unknown,
                    mapper: None,
                    diagnostics: vec![Diagnostic {
                        code: DiagCode::UnclosedTag,
                        span: Some(ByteSpan {
                            start: pos as u32,
                            end: pos as u32,
                        }),
                        message: format!("XML parse error: {err}"),
                    }],
                };
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
    let mut seen_ids: HashSet<(String, Option<String>)> = HashSet::new();
    let mut seen_fragment_ids: HashSet<String> = HashSet::new();

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
                diagnostics.push(unclosed_tag(
                    child_start as usize,
                    source.len(),
                    format!("XML parse error: {err}"),
                ));
                break;
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

                    // MM-05: lift <include> markers found in the body.
                    let (includes, mut include_diags) = lift_includes(source, dialect, &segments);
                    diagnostics.append(&mut include_diags);
                    statement.includes = includes;

                    // MM-07: normalize placeholders in each text segment to
                    // collect property_paths (from both #{}/${} forms) and
                    // surface UnterminatedPlaceholder diagnostics. Known
                    // limitation: text inside a nested dynamic tag (still
                    // an opaque DynamicTag marker at this point) isn't
                    // walked here — MM-06 recurses into those spans and
                    // will need to normalize them too.
                    let (property_paths, mut placeholder_diags) =
                        extract_property_paths(&segments, dialect);
                    diagnostics.append(&mut placeholder_diags);
                    statement.property_paths = property_paths;

                    statements.push(statement);
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

                    if let Some(mut fragment) = fragment {
                        let (includes, mut include_diags) =
                            lift_includes(source, dialect, &segments);
                        diagnostics.append(&mut include_diags);
                        fragment.includes = includes;

                        // MM-07: SqlFragment has no property_paths field in
                        // the model (fragment paths are only meaningful
                        // once inlined into a statement by MM-06), so the
                        // paths themselves are discarded here — but
                        // normalizing still surfaces diagnostics (e.g. an
                        // unterminated placeholder inside a fragment body).
                        let (_paths, mut placeholder_diags) =
                            extract_property_paths(&segments, dialect);
                        diagnostics.append(&mut placeholder_diags);

                        fragments.push(fragment);
                    }
                    if truncated {
                        break;
                    }
                    continue;
                }

                match skip_subtree(reader) {
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
                    SkipOutcome::Closed => {}
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
                }
            }
            _ => continue,
        }
    }

    // MM-05: intra-file dangling check. Only Local targets are checked —
    // Qualified (cross-file) and Dynamic (unresolvable) are the consumer's
    // job. Done after the full walk so forward references (a statement
    // above the <sql> it includes) resolve correctly.
    for statement in &statements {
        check_dangling_local_refids(&statement.includes, &seen_fragment_ids, &mut diagnostics);
    }
    for fragment in &fragments {
        check_dangling_local_refids(&fragment.includes, &seen_fragment_ids, &mut diagnostics);
    }

    // NOTE: "unused fragment" detection (spec edge case) needs
    // cross-statement <include> resolution — that's a consumer/linker
    // concern (MM-05 collects refs; nothing here resolves them), not this
    // function's job.
    let mapper = Mapper {
        namespace,
        statements,
        fragments,
        result_maps: Vec::new(),
    };
    (mapper, diagnostics)
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

    let statement = Statement {
        kind,
        id,
        database_id,
        // Placeholder — real SQL text capture lands in MM-08 (CDATA/entities)
        // and MM-06 (dynamic-tag flattening).
        sql: SqlText::Variants(Vec::new()),
        includes: Vec::new(),
        param_class: None,
        result_class: None,
        result_map_ref: None,
        property_paths: Vec::new(),
    };
    (statement, diagnostics)
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
                id,
                // Placeholder, same as Statement.sql — real text lands in
                // MM-07/MM-06.
                sql: SqlText::Variants(Vec::new()),
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

/// MM-07: runs [`crate::placeholder::normalize_segment`] over every text
/// segment in document order, collecting `property_paths` (the normalized
/// text and span_map are discarded — MM-06 regenerates them when it
/// assembles the final `SqlText`).
fn extract_property_paths(
    segments: &[BodySegment],
    dialect: Dialect,
) -> (Vec<Spanned<String>>, Vec<Diagnostic>) {
    let mut property_paths = Vec::new();
    let mut diagnostics = Vec::new();

    for segment in segments {
        let BodySegment::Text(text) = segment else {
            continue;
        };
        let mut result =
            crate::placeholder::normalize_segment(&text.decoded, text.raw_span, dialect);
        property_paths.append(&mut result.property_paths);
        diagnostics.append(&mut result.diagnostics);
    }

    (property_paths, diagnostics)
}

/// MM-05: classifies a `refid` value. `${}`-driven dynamic refids are
/// unresolvable regardless of dialect. Otherwise: MyBatis splits on the
/// *last* dot into `ns.id` (namespaces themselves may contain dots);
/// iBatis has no cross-namespace include syntax — a dot there is the
/// normal local-id convention (`WidgetDAO.commonWhere`) and always stays
/// `Local`. `raw` is preserved either way so consumers can re-derive.
fn classify_include(raw: &str, dialect: Dialect) -> IncludeTarget {
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
/// read (simple depth counting — assumes well-nested input; hostile/
/// malformed nesting is refined in MM-13).
fn skip_subtree(reader: &mut Reader<&[u8]>) -> SkipOutcome {
    let mut depth = 1u32;
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
            Err(err) => return SkipOutcome::Err(err),
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

/// MM-08: one run of decoded text plus the raw (still-escaped) byte span it
/// came from. Per invariant 4, `raw_span` slices back to the *original*
/// bytes (entities/CDATA markers unresolved); `decoded` is the resolved
/// value. The two coincide only when the segment has no entities.
struct TextSegment {
    decoded: String,
    raw_span: ByteSpan,
}

/// One unit of a statement body, in document order. Dynamic tags
/// (`<if>`, `<choose>`, iBatis `<isNotEmpty>`, ...) are recorded as opaque
/// markers — MM-06 will re-walk their span to flatten branches; MM-08 only
/// needs to not let them break text-segment ordering or span accounting.
enum BodySegment {
    Text(TextSegment),
    DynamicTag { name: String, span: ByteSpan },
}

/// Walks a statement body (from just after its own `Start` event to its
/// matching `End`), collecting [`BodySegment`]s. Returns `true` in the
/// third slot when input was truncated (EOF/parse error) — the caller
/// should stop rather than keep reading a reader that already gave up.
fn capture_body(
    source: &str,
    reader: &mut Reader<&[u8]>,
    tag_start: usize,
) -> (Vec<BodySegment>, Vec<Diagnostic>, bool) {
    let mut segments = Vec::new();
    let mut diagnostics = Vec::new();

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
                diagnostics.push(unclosed_tag(
                    child_start as usize,
                    source.len(),
                    format!("XML parse error in statement body: {err}"),
                ));
                return (segments, diagnostics, true);
            }
            Ok(Event::Text(text)) => {
                let end = reader.buffer_position() as usize;
                let raw_span = ByteSpan {
                    start: child_start as u32,
                    end: end as u32,
                };
                let decoded = match text.unescape() {
                    Ok(decoded) => decoded.into_owned(),
                    Err(err) => {
                        diagnostics.push(Diagnostic {
                            code: DiagCode::InvalidEntity,
                            span: Some(raw_span),
                            message: format!("unresolvable entity reference: {err}"),
                        });
                        // Degrade gracefully: keep the raw (still-escaped)
                        // text rather than dropping the segment.
                        source[raw_span.start as usize..raw_span.end as usize].to_string()
                    }
                };
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
                match skip_subtree(reader) {
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

/// Returns the first `name="value"` match (raw, span-preserving) plus a
/// `DuplicateAttribute` diagnostic for every repeat (recovery rule 3:
/// first value wins).
fn attr_value_spanned(
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
struct RawAttr {
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
fn scan_attributes(bytes: &[u8], tag_start: usize, tag_end: usize) -> Vec<RawAttr> {
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
        while i < tag_end && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name_end = i;

        while i < tag_end && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag_end || bytes[i] != b'=' {
            break; // malformed attribute syntax — stop rather than misparse
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
        let source = "\u{FEFF}<mapper namespace=\"x\"></mapper>";
        let result = parse_str(source);
        assert_eq!(result.dialect, Dialect::Mybatis);
        assert!(result.mapper.is_some());
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
        assert_eq!(result.diagnostics.len(), 1);
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

    /// Test harness for [`capture_body`]: `source` must be a single element
    /// whose body is what's under test, e.g. `<select id="x">...</select>`.
    fn run_capture(source: &str) -> (Vec<BodySegment>, Vec<Diagnostic>, bool) {
        let mut reader = Reader::from_str(source);
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
        let source = "<select id=\"x\">a &lt;b&gt;&amp;&quot;&apos; &#64; &#x40;</select>";
        let (segments, diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        assert!(diagnostics.is_empty());
        assert_eq!(segments.len(), 1);
        let BodySegment::Text(seg) = &segments[0] else {
            panic!("expected a text segment")
        };
        assert_eq!(seg.decoded, "a <b>&\"' @ @");
        let ByteSpan { start, end } = seg.raw_span;
        assert_eq!(
            &source[start as usize..end as usize],
            "a &lt;b&gt;&amp;&quot;&apos; &#64; &#x40;"
        );
        assert_ne!(&source[start as usize..end as usize], seg.decoded);
    }

    #[test]
    fn mm_08_undefined_entity_degrades_gracefully_with_diagnostic() {
        let source = "<select id=\"x\">a&nbsp;b</select>";
        let (segments, diagnostics, truncated) = run_capture(source);
        assert!(!truncated);
        assert_eq!(segments.len(), 1);
        let BodySegment::Text(seg) = &segments[0] else {
            panic!("expected a text segment")
        };
        // Degrades to the raw (unresolved) text rather than dropping it.
        assert_eq!(seg.decoded, "a&nbsp;b");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, DiagCode::InvalidEntity);
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
}

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
                        let (mapper, diagnostics) =
                            build_mapper(source, &mut reader, start as usize, end as usize);
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
) -> (Mapper, Vec<Diagnostic>) {
    let root_attrs = scan_attributes(source.as_bytes(), root_start, root_tag_end);
    let (namespace, mut diagnostics) = attr_value_spanned(source, &root_attrs, b"namespace");

    let mut statements = Vec::new();
    let mut seen_ids: HashSet<(String, Option<String>)> = HashSet::new();

    loop {
        let child_start = reader.buffer_position();
        match reader.read_event() {
            Ok(Event::End(_)) => break, // root closed
            Ok(Event::Eof) => {
                diagnostics.push(Diagnostic {
                    code: DiagCode::UnclosedTag,
                    span: Some(ByteSpan {
                        start: root_start as u32,
                        end: source.len() as u32,
                    }),
                    message: "root element was never closed".to_string(),
                });
                break;
            }
            Err(_) => break,
            Ok(Event::Start(tag)) => {
                let tag_end = reader.buffer_position();
                let kind = statement_kind(tag.local_name().as_ref());
                if let Some(kind) = kind {
                    let (statement, mut diags) = build_statement(
                        source,
                        kind,
                        child_start as usize,
                        tag_end as usize,
                        &mut seen_ids,
                    );
                    diagnostics.append(&mut diags);
                    statements.push(statement);
                }
                match skip_subtree(reader) {
                    SkipOutcome::Eof => {
                        diagnostics.push(Diagnostic {
                            code: DiagCode::UnclosedTag,
                            span: Some(ByteSpan {
                                start: child_start as u32,
                                end: source.len() as u32,
                            }),
                            message: "element was never closed".to_string(),
                        });
                        break;
                    }
                    SkipOutcome::Err => break,
                    SkipOutcome::Closed => {}
                }
            }
            Ok(Event::Empty(tag)) => {
                let tag_end = reader.buffer_position();
                if let Some(kind) = statement_kind(tag.local_name().as_ref()) {
                    let (statement, mut diags) = build_statement(
                        source,
                        kind,
                        child_start as usize,
                        tag_end as usize,
                        &mut seen_ids,
                    );
                    diagnostics.append(&mut diags);
                    statements.push(statement);
                }
            }
            _ => continue,
        }
    }

    let mapper = Mapper {
        namespace,
        statements,
        fragments: Vec::new(),
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
            let key = (id.value.clone(), database_id.map(|d| d.value));
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

/// Outcome of consuming a subtree after its opening `Start` event.
enum SkipOutcome {
    /// The matching `End` was found.
    Closed,
    /// Input ended before the matching `End` (recovery rule 1: the parent
    /// closing implicitly closes this element — the caller reports it).
    Eof,
    /// A parse error occurred while skipping.
    Err,
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
            Err(_) => return SkipOutcome::Err,
            _ => {}
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
}

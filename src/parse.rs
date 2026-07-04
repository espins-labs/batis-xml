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

/// MM-01: identifies the root element and derives the dialect from its
/// name (`<mapper>` → MyBatis, `<sqlMap>` → iBatis).
pub(crate) fn parse_str(source: &str) -> ParseResult {
    let mut reader = Reader::from_str(source);

    loop {
        let start = reader.buffer_position();
        match reader.read_event() {
            Ok(Event::Start(tag)) | Ok(Event::Empty(tag)) => {
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
    let bytes = source.as_bytes();
    let attrs = scan_attributes(bytes, tag_start, tag_end);
    let mut matches = attrs
        .iter()
        .filter(|a| &bytes[a.name.0..a.name.1] == b"namespace");

    let namespace = matches.next().map(|attr| Spanned {
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
            message: "duplicate 'namespace' attribute; first value wins".to_string(),
        })
        .collect();

    let mapper = Mapper {
        namespace,
        statements: Vec::new(),
        fragments: Vec::new(),
        result_maps: Vec::new(),
    };
    (mapper, diagnostics)
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
}

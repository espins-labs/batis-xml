//! MM-12: property tests over a synthetic valid-mapper generator plus
//! arbitrary-byte inputs.
//!
//! Invariants (per spec):
//! 1. Never panics (a Rust panic fails the proptest case directly).
//! 2. `parse`/`parse_bytes` is deterministic — same input, same output.
//! 3. Every `ByteSpan` is in-range for the source it was produced from.
//! 4. Every `SqlString.span_map` is strictly increasing (by synthetic
//!    offset) within a single variant.
//! 5. For property-path values that are plainly verbatim identifiers (no
//!    entity decoding could have touched them), the span slices back to
//!    exactly that value in the original source.

use batis_xml::{ByteSpan, Dialect, ParseResult, SqlText};
use proptest::prelude::*;
use proptest::strategy::ValueTree;

// ---------------------------------------------------------------------
// Generator: valid mapper XML (spec sketch — statements 0..10, dynamic
// nesting 0..4, branch counts spanning the N=32 cap, identifiers
// including Korean and `_`, CDATA/entities ~50%, dialect toggle).
// ---------------------------------------------------------------------

fn ascii_ident() -> impl Strategy<Value = String> {
    "[a-zA-Z_][a-zA-Z0-9_]{0,10}"
}

/// A handful of fixed Korean identifiers (Hangul syllables) — enough to
/// exercise multibyte identifiers without needing a char-range strategy
/// wired through every call site.
fn korean_ident() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        "위젯".to_string(),
        "이름".to_string(),
        "상태값".to_string(),
        "그룹코드".to_string(),
    ])
}

fn identifier() -> impl Strategy<Value = String> {
    prop_oneof![3 => ascii_ident(), 1 => korean_ident()]
}

/// A leaf text fragment: plain SQL-ish text, a `#{prop}` placeholder
/// reference, optionally CDATA-wrapped, optionally carrying an entity.
fn text_leaf() -> impl Strategy<Value = String> {
    (identifier(), any::<bool>(), any::<bool>()).prop_map(|(prop, use_cdata, use_entity)| {
        let body = if use_entity {
            format!("col {prop} &lt;= #{{{prop}}} &amp; 1")
        } else {
            format!("col_{prop} = #{{{prop}}}")
        };
        if use_cdata {
            format!("<![CDATA[ {body} ]]>")
        } else {
            format!(" {body} ")
        }
    })
}

fn condition() -> impl Strategy<Value = String> {
    identifier().prop_map(|id| format!("{id} != null"))
}

/// Recursively builds a MyBatis-flavored dynamic-tag fragment, up to
/// `max_depth` levels of `<if>`/`<choose>` nesting. `max_depth == 0` only
/// produces leaf text (the recursion floor).
fn mybatis_dynamic(max_depth: u32) -> BoxedStrategy<String> {
    let leaf = text_leaf().boxed();
    if max_depth == 0 {
        return leaf;
    }
    let inner = mybatis_dynamic(max_depth - 1);
    prop_oneof![
        3 => leaf,
        2 => (condition(), mybatis_dynamic(max_depth - 1))
            .prop_map(|(cond, body)| format!(r#"<if test="{cond}">{body}</if>"#)),
        1 => (prop::collection::vec((condition(), inner), 1..4), any::<bool>()).prop_map(
            |(whens, has_otherwise)| {
                let mut s = String::from("<choose>");
                for (cond, body) in whens {
                    s.push_str(&format!(r#"<when test="{cond}">{body}</when>"#));
                }
                if has_otherwise {
                    s.push_str("<otherwise>fallback = 1</otherwise>");
                }
                s.push_str("</choose>");
                s
            }
        ),
    ]
    .boxed()
}

/// A statement body designed to span the N=32 branch cap: 0..6 sibling
/// `<if>` tags (2^0=1 .. 2^6=64), each itself recursively nested up to
/// `max_depth`.
fn statement_body(max_depth: u32) -> impl Strategy<Value = String> {
    prop::collection::vec((condition(), mybatis_dynamic(max_depth)), 0..6).prop_map(|siblings| {
        let mut s = String::from("SELECT 1");
        for (cond, body) in siblings {
            s.push_str(&format!(r#"<if test="{cond}">{body}</if>"#));
        }
        s
    })
}

fn statement_kind_tag() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["select", "insert", "update", "delete"])
}

fn mybatis_statement() -> impl Strategy<Value = String> {
    (statement_kind_tag(), identifier(), statement_body(4))
        .prop_map(|(kind, id, body)| format!(r#"<{kind} id="{id}">{body}</{kind}>"#))
}

fn ibatis_statement() -> impl Strategy<Value = String> {
    (identifier(), identifier(), any::<bool>()).prop_map(|(dao, method, use_cdata)| {
        let body = format!("col = #{{{method}}}#")
            .replace("#{", "#")
            .replace("}#", "#");
        let body = if use_cdata {
            format!("<![CDATA[ {body} ]]>")
        } else {
            body
        };
        format!(r#"<select id="{dao}.{method}">{body}</select>"#)
    })
}

/// Full mapper document: dialect toggle, 0..10 statements.
fn mapper_document() -> impl Strategy<Value = String> {
    any::<bool>().prop_flat_map(|is_mybatis| {
        if is_mybatis {
            prop::collection::vec(mybatis_statement(), 0..10)
                .prop_map(|stmts| {
                    format!(
                        r#"<mapper namespace="com.example.Gen">{}</mapper>"#,
                        stmts.concat()
                    )
                })
                .boxed()
        } else {
            prop::collection::vec(ibatis_statement(), 0..10)
                .prop_map(|stmts| format!("<sqlMap>{}</sqlMap>", stmts.concat()))
                .boxed()
        }
    })
}

// ---------------------------------------------------------------------
// Invariant checkers
// ---------------------------------------------------------------------

fn assert_span_in_range(start: u32, end: u32, len: usize, ctx: &str) {
    assert!(start <= end, "{ctx}: span start {start} > end {end}");
    assert!(
        end as usize <= len,
        "{ctx}: span end {end} exceeds source length {len}"
    );
}

fn check_span_map_strictly_increasing(span_map: &[(u32, u32)], ctx: &str) {
    assert!(
        span_map.windows(2).all(|w| w[0].0 < w[1].0),
        "{ctx}: span_map offsets not strictly increasing: {span_map:?}"
    );
}

/// Statement/SqlFragment/ResultMap.span (opening-tag start -> subtree end)
/// must contain every span nested inside it -- a child pointing outside
/// its own enclosing tag would mean the offsets are simply wrong.
fn assert_span_contains(outer: ByteSpan, inner: ByteSpan, ctx: &str) {
    assert!(
        outer.start <= inner.start && inner.end <= outer.end,
        "{ctx}: inner span {inner:?} not contained in outer span {outer:?}"
    );
}

/// Walks the whole `ParseResult`, checking invariants 3, 4, and (for
/// plainly-verbatim-identifier property paths) 5.
fn check_result(source: &str, result: &ParseResult) {
    let len = source.len();

    for d in &result.diagnostics {
        if let Some(span) = d.span {
            assert_span_in_range(span.start, span.end, len, "diagnostic span");
        }
    }

    let Some(mapper) = &result.mapper else {
        return;
    };

    if let Some(ns) = &mapper.namespace {
        assert_span_in_range(ns.span.start, ns.span.end, len, "namespace span");
    }

    for stmt in &mapper.statements {
        assert_span_in_range(stmt.span.start, stmt.span.end, len, "statement span");
        if let Some(id) = &stmt.id {
            assert_span_in_range(id.span.start, id.span.end, len, "statement id span");
            assert_span_contains(stmt.span, id.span, "statement span vs id span");
        }
        if let Some(db) = &stmt.database_id {
            assert_span_in_range(db.span.start, db.span.end, len, "database_id span");
            assert_span_contains(stmt.span, db.span, "statement span vs database_id span");
        }
        for include in &stmt.includes {
            assert_span_in_range(include.span.start, include.span.end, len, "include span");
            assert_span_contains(stmt.span, include.span, "statement span vs include span");
        }
        for path in &stmt.property_paths {
            assert_span_in_range(path.span.start, path.span.end, len, "property_path span");
            assert_span_contains(stmt.span, path.span, "statement span vs property_path span");
            check_verbatim_property_path(source, path);
        }
        check_sql_text(source, &stmt.sql);
    }

    for fragment in &mapper.fragments {
        assert_span_in_range(fragment.span.start, fragment.span.end, len, "fragment span");
        assert_span_in_range(
            fragment.id.span.start,
            fragment.id.span.end,
            len,
            "fragment id span",
        );
        assert_span_contains(fragment.span, fragment.id.span, "fragment span vs id span");
        for include in &fragment.includes {
            assert_span_in_range(
                include.span.start,
                include.span.end,
                len,
                "fragment include span",
            );
            assert_span_contains(fragment.span, include.span, "fragment span vs include span");
        }
        check_sql_text(source, &fragment.sql);
    }

    for rm in &mapper.result_maps {
        assert_span_in_range(rm.span.start, rm.span.end, len, "resultMap span");
        assert_span_in_range(rm.id.span.start, rm.id.span.end, len, "resultMap id span");
        assert_span_contains(rm.span, rm.id.span, "resultMap span vs id span");
    }
}

fn check_sql_text(source: &str, sql: &SqlText) {
    let len = source.len();
    match sql {
        SqlText::Variants(variants) => {
            for v in variants {
                check_span_map_strictly_increasing(&v.text.span_map, "variant span_map");
                for (off, raw) in &v.text.span_map {
                    assert!(
                        (*off as usize) <= v.text.text.len(),
                        "span_map synthetic offset {off} exceeds text length {}",
                        v.text.text.len()
                    );
                    assert!(
                        (*raw as usize) <= len,
                        "span_map raw offset {raw} exceeds source length {len}"
                    );
                }
            }
        }
        SqlText::Union { text, .. } => {
            check_span_map_strictly_increasing(&text.span_map, "union span_map");
        }
    }
}

/// Invariant 5, applied where we can tell (from the value's own shape)
/// that no entity decoding could have been involved: a value made only of
/// ASCII word characters is exactly what our generator's `#{ident}`
/// placeholders produce verbatim, with no `&...;` anywhere nearby.
fn check_verbatim_property_path(source: &str, path: &batis_xml::Spanned<String>) {
    let looks_verbatim = !path.value.is_empty()
        && path
            .value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !looks_verbatim {
        return; // Korean identifiers and anything else: skip (see doc comment).
    }
    if path.span.start == path.span.end {
        // A zero-width span is placeholder.rs's documented coarse-fallback
        // marker for a segment that had entities elsewhere in it (even if
        // this particular path's text didn't need decoding) — not a claim
        // of precision, so nothing to check here.
        return;
    }
    let start = path.span.start as usize;
    let end = path.span.end as usize;
    if end > source.len() {
        return; // already reported by the range check
    }
    assert_eq!(
        &source[start..end],
        path.value.as_str(),
        "verbatim property_path span doesn't slice back to its own value"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Invariants 1 (no panic — implicit), 2 (deterministic), 3, 4, 5.
    #[test]
    fn mm_12_valid_mapper_generator_upholds_invariants(source in mapper_document()) {
        let first = batis_xml::parse(&source);
        let second = batis_xml::parse(&source);
        prop_assert_eq!(&first, &second, "parse(source) is not deterministic");
        check_result(&source, &first);
    }

    /// Invariant 1 (no panic) + 2 (deterministic) over arbitrary bytes —
    /// this is the input that's least likely to look anything like XML.
    #[test]
    fn mm_12_arbitrary_bytes_never_panics_and_is_deterministic(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
        let first = batis_xml::parse_bytes(&bytes);
        let second = batis_xml::parse_bytes(&bytes);
        prop_assert_eq!(first, second, "parse_bytes(bytes) is not deterministic");
    }

    /// Same as above, but biased toward looking XML-ish (more `<`, `>`,
    /// `"`, `/` bytes) so the fuzzer spends more time near the parser's
    /// actual decision points instead of pure noise.
    #[test]
    fn mm_12_xml_ish_arbitrary_bytes_never_panics(
        bytes in prop::collection::vec(
            prop_oneof![
                5 => Just(b'<'), 5 => Just(b'>'), 3 => Just(b'"'), 3 => Just(b'/'),
                3 => Just(b'='), 2 => Just(b'&'), 2 => Just(b';'),
                10 => any::<u8>(),
            ],
            0..1024,
        )
    ) {
        let _ = batis_xml::parse_bytes(&bytes);
    }
}

#[test]
fn mm_12_dialect_toggle_generator_produces_both_dialects() {
    // Sanity check on the generator itself, not the parser: run it a
    // fixed number of times and confirm both branches are reachable.
    let mut runner = proptest::test_runner::TestRunner::default();
    let mut saw_mybatis = false;
    let mut saw_ibatis = false;
    for _ in 0..50 {
        let value = mapper_document().new_tree(&mut runner).unwrap().current();
        let result = batis_xml::parse(&value);
        match result.dialect {
            Dialect::Mybatis => saw_mybatis = true,
            Dialect::Ibatis => saw_ibatis = true,
            Dialect::Unknown => {}
        }
    }
    assert!(
        saw_mybatis && saw_ibatis,
        "generator should hit both dialects across 50 samples"
    );
}

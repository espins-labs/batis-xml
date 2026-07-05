//! Output model — the serde serialization of these types IS the published
//! schema (`schema/batis-xml.v1.json`, pinned by a snapshot test).
//!
//! Spec: spec-mybatis-mapper.md "Public API (final)". Removing or renaming
//! a field, or removing/renaming an enum variant, is always breaking.
//! Additions are **additive at the JSON-representation level** (schema
//! v1 stays valid; old consumers keep working against new output) --
//! this is *not* the same as "non-breaking" at the Rust type level: a
//! struct field addition or enum variant addition is a 0.x minor-semver
//! bump, since an exhaustive `match` outside this crate must be updated
//! (for the `#[non_exhaustive]` enums below, the compiler enforces this
//! with a wildcard-arm requirement; plain structs stay constructible by
//! downstream test harnesses, so they don't get `#[non_exhaustive]`).
//! Cold code-review contract audit, code-atlas 8e4ed9a, 2026-07-05.

use serde::{Deserialize, Serialize};

/// Half-open range `[start, end)`: byte offsets into the UTF-8 text as
/// decoded by this crate (identical to raw input bytes for UTF-8 sources;
/// see the caveat below for re-encoded documents). B23 (cold code review):
/// the previous headline -- "in original bytes -- never in the decoded
/// string" -- directly contradicted its own caveat below for every
/// non-UTF-8 input, since spans on a re-encoded document *are* offsets
/// into the decoded string, not the original raw bytes.
///
/// Caveat: this holds exactly for UTF-8 input, which decoding leaves
/// byte-for-byte unchanged. For documents decoded from any other
/// encoding (EUC-KR, Shift_JIS, GB18030, UTF-16, ... -- see `encoding.rs`,
/// which supports every WHATWG encoding via a BOM/declared-label-driven
/// chain, not just EUC-KR), decoding to UTF-8 changes byte *widths* per
/// character, so spans on such documents are offsets into the re-encoded
/// UTF-8 string, not the original raw bytes. This applies uniformly to
/// every re-encoded document, not just Korean legacy files. Consumers
/// reading spans back against a source file must decode that source the
/// same way this crate did before slicing.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteSpan {
    pub start: u32,
    pub end: u32,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spanned<T> {
    pub value: T,
    pub span: ByteSpan,
}

/// Closed set: an exhaustive `match` is a consumer feature (there's no
/// forward-compat concern the way there is for `DiagCode`/`SqlText`,
/// since `Unknown` already covers "neither of the two known dialects").
/// Adding a third dialect would be a v2.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    Mybatis,
    Ibatis,
    Unknown,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParseResult {
    pub dialect: Dialect,
    /// `None` when the root element is not a mapper/sqlMap (reason is
    /// reported as a diagnostic).
    pub mapper: Option<Mapper>,
    /// Parsing never fails — every anomaly accumulates here.
    pub diagnostics: Vec<Diagnostic>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: DiagCode,
    pub span: Option<ByteSpan>,
    pub message: String,
}

/// Additions only; removal/renaming is breaking. `#[non_exhaustive]`
/// because new codes may appear within v1 itself (see `schema/README.md`)
/// -- an exhaustive `match` outside this crate must add a wildcard arm,
/// which is exactly the forward-compat behavior consumers need (an
/// unrecognized code is not an error). `Other` covers the equivalent case
/// for *deserialization* (this build reading JSON produced by a newer
/// version).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagCode {
    EncodingUndetectable,
    EncodingMismatch,
    UnclosedTag,
    DuplicateStatementId,
    MissingStatementId,
    /// A **file-local heuristic**: this crate parses one mapper file at a
    /// time and has no view of any other file's `<sql>` fragments, so this
    /// is only emitted for MyBatis (`Dialect::Mybatis`), whose namespaces
    /// are per-file and whose refids are typically resolved within them.
    /// Never emitted for iBatis (B22, cold code review) -- iBatis fragments
    /// are a global cross-file registry by design (any sqlMap can reference
    /// any other sqlMap's `<sql>` by short name), so this heuristic would
    /// flag nearly every legitimate cross-file reference as dangling.
    /// Consumers that resolve `<include>` across an entire project (rather
    /// than one file) should treat a *missing* `DanglingRefid` as "not
    /// checked here", not "resolved" -- upstream MyBatis also supports
    /// cross-namespace short-name resolution that this single-file view
    /// can't see either, so even the MyBatis case is a heuristic, not a
    /// guarantee.
    DanglingRefid,
    BranchLimitExceeded,
    UnknownElement,
    /// Input exceeded [`crate::MAX_INPUT_BYTES`] (B25, cold code review: was
    /// only documented as "the 10 MB cap" in prose; now also a public,
    /// checkable constant). Emitted by both `parse`/`parse_bytes` (with
    /// `mapper: None`) and `detect_dialect` (with `Dialect::Unknown`)
    /// *before* any decoding is attempted, so the cap applies to raw input
    /// size regardless of encoding.
    OversizeInput,
    /// Recovery rule 3: first value wins, duplicate is reported here.
    DuplicateAttribute,
    /// An entity reference could not be resolved (e.g. `&nbsp;`, common in
    /// legacy mappers). The raw text is kept as-is for that segment (MM-08).
    InvalidEntity,
    /// A `#{`/`${`/legacy `#..#`/`$..$` placeholder never found its closing
    /// delimiter within the segment. The raw text is kept as-is (MM-07).
    UnterminatedPlaceholder,
    /// Recursion depth exceeded 256 nesting levels (dynamic-tag flattening
    /// or resultMap association/discriminator nesting) -- the remaining
    /// subtree is treated as opaque (no text/mapping contribution) rather
    /// than risk a stack overflow. Cold code review B2/B3, 2026-07-05.
    NestingLimitExceeded,
    /// An `<include>` is the first or last non-whitespace content directly
    /// inside a `<where>`/`<set>`/`<trim>` wrapper. MyBatis expands
    /// `<include>` *before* dynamic evaluation, so the wrapper's own
    /// leading-AND/OR or trailing-comma rule sees the fragment's actual
    /// substituted text; this crate flattens with the include token still
    /// in place, so a consumer substituting the fragment afterward must
    /// re-apply that rule themselves (and treat a wrapper whose only
    /// content is this include as conditional, since the fragment may
    /// expand to nothing). See the README's include section and
    /// `IncludeTarget`'s rustdoc. Cold code review A7, 2026-07-05.
    IncludeAtWrapperBoundary,
    /// Forward-compat deserialization fallback: any code string this build
    /// doesn't recognize (e.g. JSON produced by a newer batis-xml version)
    /// lands here instead of failing to deserialize. Never produced by
    /// this crate's own parser -- hidden from docs since it's a
    /// deserialization mechanism, not a diagnostic you'd match on
    /// intentionally. Cold code-review contract audit, 2026-07-05.
    #[doc(hidden)]
    #[serde(other)]
    Other,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mapper {
    /// Usually `None` for iBatis sqlMaps (observed in the wild: the prefix
    /// lives inside the statement id itself, e.g. `WidgetDAO.getWidget`).
    pub namespace: Option<Spanned<String>>,
    pub statements: Vec<Statement>,
    pub fragments: Vec<SqlFragment>,
    pub result_maps: Vec<ResultMap>,
}

/// Closed set: an exhaustive `match` is a consumer feature. `Generic`
/// already covers "some other statement-like tag with no CRUD-verb
/// equivalent" -- a new MyBatis/iBatis statement-like tag would be
/// recognized by adding to `Generic`'s callers, not by growing this enum.
/// Adding a variant here would be a v2.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    Select,
    Insert,
    Update,
    Delete,
    Procedure,
    /// iBatis `<statement>` — a generic statement tag with no MyBatis
    /// CRUD-verb equivalent.
    Generic,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    pub kind: StatementKind,
    /// Full original extent of the statement: opening-tag start → subtree
    /// end (i.e. past its closing tag, or its own end for a self-closed
    /// element).
    #[serde(default)]
    pub span: ByteSpan,
    /// `None` when missing, plus a `MissingStatementId` diagnostic.
    /// Synthesized ids are never invented.
    pub id: Option<Spanned<String>>,
    /// MyBatis per-vendor branching (`databaseId="oracle"` etc.) — `None`
    /// when the statement doesn't declare one. Distinguishes otherwise
    /// duplicate ids (MM-03).
    #[serde(default)]
    pub database_id: Option<Spanned<String>>,
    pub sql: SqlText,
    pub includes: Vec<Spanned<IncludeRef>>,
    pub param_class: Option<Spanned<ClassRef>>,
    pub result_class: Option<Spanned<ClassRef>>,
    pub result_map_ref: Option<Spanned<String>>,
    /// Expression paths collected from `#{a.b}` / `${c}` (MM-07).
    pub property_paths: Vec<Spanned<String>>,
}

/// Result of dynamic-tag flattening (MM-06).
/// Branch combination cap N=32 — total candidates per statement, computed
/// as the cartesian product of tag branches.
///
/// `#[non_exhaustive]`: a third fallback representation is plausible
/// future work, and consumers matching exhaustively today must not break
/// at compile time if one's added.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SqlText {
    Variants(Vec<SqlVariant>),
    /// Over-cap fallback (accompanied by a `BranchLimitExceeded` diagnostic).
    Union {
        text: SqlString,
        /// A **lower bound**, not necessarily the exact branch count:
        /// flattening bails out of cartesian expansion as soon as it's
        /// certain the total exceeds the cap, without finishing the
        /// (possibly much larger) exact count. Treat this as "at least
        /// this many, over the cap" rather than a precise total.
        branch_count: u32,
    },
    /// Forward-compat deserialization fallback: any `SqlText` shape this
    /// build's `Deserialize` impl doesn't recognize (e.g. produced by a
    /// future batis-xml version) lands here instead of failing to
    /// deserialize the whole document. Never produced by this crate's own
    /// flattening -- hidden from docs since it's a deserialization
    /// mechanism, not a shape to construct or match on intentionally.
    /// Serializing it back out degrades: the original unrecognized key and
    /// value aren't retained, only the fact that *something* unrecognized
    /// was there. Acceptable since this crate never produces the variant
    /// itself -- it only exists transiently after reading a newer
    /// version's output. Excluded from the JSON Schema (`schemars(skip)`):
    /// it's not a shape this crate's own output can ever contain, so it
    /// has no business in the *published* schema. Cold code review A8,
    /// 2026-07-05.
    #[doc(hidden)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unrecognized,
}

/// Manual `Deserialize` for `SqlText` (A8, cold code review) -- this can't
/// be `#[derive(Deserialize)]`.
///
/// `SqlText` is externally tagged (`{"variants": [...]}` / `{"union":
/// {...}}`), and `#[serde(other)]` -- the mechanism `DiagCode`/
/// `IncludeTarget` use for their own forward-compat fallback -- only works
/// on unit variants of internally/adjacently tagged enums, not externally
/// tagged ones like this. Without a workaround, a document produced by a
/// future version with a `SqlText` shape this build doesn't know about
/// would fail deserialization for the *whole document*, contradicting the
/// schema/README's promise that unrecognized shapes are soft-fail within
/// v1. Deliberately kept out of `SqlText`'s own doc comment (which
/// `schemars` reads into the published schema's description) -- this is an
/// implementation rationale, not part of the public contract, and the
/// schema for this crate's own output is unaffected either way (see
/// `SqlText::Unrecognized`'s `schemars(skip)`).
///
/// Reads the externally tagged shape (`{"variants": ...}` / `{"union":
/// ...}`) as a single-key map; an unrecognized key is consumed with
/// `IgnoredAny` (no `serde_json` dependency -- this works against any
/// `serde` `Deserializer`, not just a JSON-specific value type) rather than
/// erroring.
impl<'de> Deserialize<'de> for SqlText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct UnionRepr {
            text: SqlString,
            branch_count: u32,
        }

        struct SqlTextVisitor;

        impl<'de> serde::de::Visitor<'de> for SqlTextVisitor {
            type Value = SqlText;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(
                    "a SqlText representation (`variants` or `union`, or an \
                     unrecognized single-key map from a future version)",
                )
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let key: String = match map.next_key()? {
                    Some(key) => key,
                    None => {
                        return Err(serde::de::Error::invalid_length(0, &"a single-key map"));
                    }
                };
                match key.as_str() {
                    "variants" => Ok(SqlText::Variants(map.next_value()?)),
                    "union" => {
                        let union: UnionRepr = map.next_value()?;
                        Ok(SqlText::Union {
                            text: union.text,
                            branch_count: union.branch_count,
                        })
                    }
                    _ => {
                        // Forward-compat: some other SqlText shape this
                        // build doesn't recognize -- consume and discard
                        // its value rather than failing the whole document.
                        map.next_value::<serde::de::IgnoredAny>()?;
                        Ok(SqlText::Unrecognized)
                    }
                }
            }
        }

        deserializer.deserialize_map(SqlTextVisitor)
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SqlVariant {
    pub text: SqlString,
    /// The `test` expressions (verbatim) that activate this variant.
    pub conditions: Vec<String>,
}

/// Flattened SQL text plus a mapping back to the source.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SqlString {
    /// Placeholders already normalized: `#{..}` → `?`, `${..}` → `__BATIS_DYN__`.
    pub text: String,
    /// (synthetic-text offset, original byte offset) segment-start pairs —
    /// strictly increasing.
    pub span_map: Vec<(u32, u32)>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SqlFragment {
    /// Full original extent: opening-tag start → subtree end.
    #[serde(default)]
    pub span: ByteSpan,
    pub id: Spanned<String>,
    pub sql: SqlText,
    /// Nested includes inside the fragment (MM-04).
    pub includes: Vec<Spanned<IncludeRef>>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IncludeRef {
    pub raw: String,
    pub target: IncludeTarget,
}

/// Closed set: an exhaustive `match` is a consumer feature. `Dynamic`
/// already covers "can't be resolved statically" -- there's no third kind
/// of refid target. Adding a variant here would be a v2.
///
/// ## Expansion-order contract (A7, cold code review)
///
/// This crate never substitutes the referenced `<sql>` fragment's text in
/// place of the `<include>` token -- resolving `IncludeTarget` to actual
/// SQL and splicing it in is entirely the consumer's job. MyBatis/iBatis
/// themselves expand `<include>` *before* evaluating `<where>`/`<set>`/
/// `<trim>` dynamic semantics, so a wrapper's leading-AND/OR strip or
/// trailing-comma strip sees the fragment's real, substituted text.
/// Flattening here with the token still in place means a consumer
/// substituting fragment text in afterward must, at minimum:
///
/// - re-apply the wrapper's leading-AND/OR / trailing-comma cleanup to the
///   substituted text when the include token was first/last inside a
///   `<where>`/`<set>`/`<trim>`, and
/// - treat a wrapper whose only content is an include token as
///   conditional (the fragment may expand to nothing).
///
/// `DiagCode::IncludeAtWrapperBoundary` flags exactly the spots this
/// applies to -- see the README's "Include expansion order" section for
/// the full write-up.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncludeTarget {
    Local(String),
    Qualified {
        ns: String,
        id: String,
    },
    /// `${}`-driven refid — marked unresolvable.
    Dynamic,
}

/// Alias resolution is the consumer's job — only the raw text is kept.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassRef {
    pub raw: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultMap {
    /// Full original extent: opening-tag start → subtree end.
    #[serde(default)]
    pub span: ByteSpan,
    pub id: Spanned<String>,
    pub type_ref: Option<Spanned<ClassRef>>,
    pub extends: Option<Spanned<String>>,
    pub mappings: Vec<ColumnMapping>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnMapping {
    pub column: Option<String>,
    pub property: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // A3 (cold code review): DiagCode's #[serde(other)] Other variant is
    // the forward-compat escape hatch for deserializing JSON produced by a
    // future version with a code this build doesn't know about yet.

    #[test]
    fn diag_code_deserializes_unknown_string_to_other() {
        let json =
            r#"{"code":"some_future_code_this_build_does_not_know","span":null,"message":"x"}"#;
        let d: Diagnostic = serde_json::from_str(json).expect("deserializes, doesn't fail");
        assert_eq!(d.code, DiagCode::Other);
    }

    #[test]
    fn diag_code_still_deserializes_known_codes_normally() {
        let json = r#"{"code":"unclosed_tag","span":null,"message":"x"}"#;
        let d: Diagnostic = serde_json::from_str(json).expect("deserializes");
        assert_eq!(d.code, DiagCode::UnclosedTag);
    }

    // A8 (cold code review): SqlText is externally tagged, so
    // #[serde(other)] (DiagCode's mechanism above) doesn't apply --
    // #[serde(other)] only works on unit variants of internally/adjacently
    // tagged enums. The manual Deserialize impl provides the same
    // forward-compat soft-fail via a hand-rolled single-key-map visitor.

    #[test]
    fn sql_text_deserializes_unknown_representation_to_unrecognized() {
        let json = r#"{"some_future_shape":{"anything":"goes","nested":[1,2,3]}}"#;
        let sql: SqlText = serde_json::from_str(json).expect("deserializes, doesn't fail");
        assert_eq!(sql, SqlText::Unrecognized);
    }

    #[test]
    fn sql_text_still_deserializes_variants_normally() {
        let json =
            r#"{"variants":[{"text":{"text":"SELECT 1","span_map":[[0,0]]},"conditions":[]}]}"#;
        let sql: SqlText = serde_json::from_str(json).expect("deserializes");
        match sql {
            SqlText::Variants(variants) => {
                assert_eq!(variants.len(), 1);
                assert_eq!(variants[0].text.text, "SELECT 1");
            }
            other => panic!("expected Variants, got {other:?}"),
        }
    }

    #[test]
    fn sql_text_still_deserializes_union_normally() {
        let json = r#"{"union":{"text":{"text":"SELECT 1","span_map":[[0,0]]},"branch_count":33}}"#;
        let sql: SqlText = serde_json::from_str(json).expect("deserializes");
        match sql {
            SqlText::Union { text, branch_count } => {
                assert_eq!(text.text, "SELECT 1");
                assert_eq!(branch_count, 33);
            }
            other => panic!("expected Union, got {other:?}"),
        }
    }

    #[test]
    fn sql_text_unrecognized_round_trips_through_serialize() {
        // Never produced by this crate's own parsing -- only reachable by
        // deserializing a future version's output -- but must still
        // serialize back out without panicking (degraded: the original
        // unrecognized shape isn't retained).
        let json = r#"{"totally_new_shape":42}"#;
        let sql: SqlText = serde_json::from_str(json).expect("deserializes");
        let round_tripped = serde_json::to_string(&sql).expect("serializes");
        assert_eq!(round_tripped, "\"unrecognized\"");
    }
}

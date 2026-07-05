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

/// Half-open range `[start, end)` in **original bytes** — never in the
/// decoded string.
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
    DanglingRefid,
    BranchLimitExceeded,
    UnknownElement,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

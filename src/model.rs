//! Output model — the serde serialization of these types IS the published
//! schema (`schema/batis-xml.v1.json`, to be pinned by snapshot tests).
//!
//! Spec: spec-mybatis-mapper.md "Public API (final)". Removing or renaming
//! fields is breaking; additions are non-breaking (including `DiagCode`).

use serde::{Deserialize, Serialize};

/// Half-open range `[start, end)` in **original bytes** — never in the
/// decoded string.
///
/// Caveat: this holds exactly for UTF-8 input, which decoding leaves
/// byte-for-byte unchanged. For documents that were re-encoded from
/// EUC-KR/CP949 (see `encoding.rs`), decoding to UTF-8 changes byte
/// *widths* per character, so spans on such documents are offsets into the
/// re-encoded UTF-8 string, not the original raw bytes. Consumers reading
/// spans back against a source file must decode that source the same way
/// this crate did before slicing.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Additions only; removal/renaming is breaking.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlText {
    Variants(Vec<SqlVariant>),
    /// Over-cap fallback (accompanied by a `BranchLimitExceeded` diagnostic).
    Union {
        text: SqlString,
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
    /// Placeholders already normalized: `#{..}` → `?`, `${..}` → `__ATLAS_DYN__`.
    pub text: String,
    /// (synthetic-text offset, original byte offset) segment-start pairs —
    /// strictly increasing.
    pub span_map: Vec<(u32, u32)>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SqlFragment {
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

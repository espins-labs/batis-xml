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

use serde::{Deserialize, Serialize};

/// Half-open range `[start, end)`: byte offsets into the UTF-8 text as
/// decoded by this crate (identical to raw input bytes for UTF-8 sources;
/// see the caveat below for re-encoded documents).
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
///
/// A leading byte-order mark is never part of this text either way: a
/// BOM is consumed during decoding for [`crate::parse_bytes`] (see
/// [`ParseResult::encoding`]'s doc comment) and stripped from the input
/// string itself for [`crate::parse`] (a caller can hand it an
/// already-decoded string that still carries a BOM, e.g. read from a
/// file without stripping it first) -- both entry points agree that
/// every span is relative to the BOM-stripped content.
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
    /// The WHATWG name of the encoding the detection chain actually
    /// decoded the input with (`"UTF-8"`, `"EUC-KR"`, `"Shift_JIS"`,
    /// `"UTF-16LE"`, ...; `encoding_rs::Encoding::name()`'s own values,
    /// which are WHATWG-standard labels). `None` only when no decode was
    /// attempted at all (the raw-byte oversize cap rejected the input
    /// before `encoding.rs` ever ran) -- `parse` (already-decoded `&str`
    /// input) always reports `"UTF-8"`, since that's the one encoding a
    /// Rust `&str` can ever be.
    ///
    /// This is what makes the [`ByteSpan`] re-encoding caveat actionable:
    /// every span in this result is a byte offset into the UTF-8 text
    /// *after* decoding, so a consumer working with the **original**
    /// input bytes must decode them the same way first --
    /// `new TextDecoder(result.encoding)` (Node.js/browsers both accept
    /// WHATWG labels directly), re-encode that decoded text to UTF-8, and
    /// slice spans against *that* buffer, not the original input bytes
    /// directly (see `wasm/README.md` for a worked recipe).
    ///
    /// BOM handling: for a document that opened with a byte-order mark,
    /// the mark is consumed during decoding and never appears in the
    /// decoded text -- spans are relative to the BOM-stripped content,
    /// same as every other span in this crate (offsets into what this
    /// crate's own decoding produced, not the original file's raw byte
    /// layout). A consumer re-decoding the original file with
    /// `TextDecoder` gets BOM-stripping for free (that's standard
    /// `TextDecoder` behavior for UTF-8/UTF-16 with a matching BOM), so no
    /// extra adjustment is needed on the consumer's side either.
    pub encoding: Option<String>,
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
    /// Never emitted for iBatis -- iBatis fragments
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
    /// Input exceeded [`crate::MAX_INPUT_BYTES`]. Emitted by both
    /// `parse`/`parse_bytes` (with `mapper: None`) and `detect_dialect`
    /// (with `Dialect::Unknown`) *before* any decoding is attempted, so
    /// the cap applies to raw input size regardless of encoding.
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
    /// than risk a stack overflow.
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
    /// `IncludeTarget`'s rustdoc.
    IncludeAtWrapperBoundary,
    /// Forward-compat deserialization fallback: any code string this
    /// build doesn't recognize (e.g. JSON produced by a newer batis-xml
    /// version) lands here instead of failing to deserialize the whole
    /// document. Never produced by this crate's own parser -- it's part
    /// of the deserialization contract (see this enum's own doc comment),
    /// not a diagnostic this crate ever emits itself, so treat a `match`
    /// arm on it as "some future code I don't know about yet", never as
    /// a specific, actionable anomaly.
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
// A11 (cold code review): `Serialize`/`Deserialize` are hand-rolled below
// (see their impls' own doc comments), but `#[serde(rename_all =
// "snake_case")]` stays here anyway -- `schemars` reads it directly to
// decide the *schema's* variant-name casing ("variants"/"union", matching
// what the manual `Serialize` impl actually produces), independent of
// whether serde's own derive macros are present.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", serde(rename_all = "snake_case"))]
#[derive(Debug, Clone, PartialEq)]
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
    /// Serializes as `{"unrecognized": null}` -- the original unrecognized
    /// key and value aren't retained, only the fact that *something*
    /// unrecognized was there, but the shape is a single-key map like
    /// every other `SqlText` variant, so it survives its own round trip
    /// (serialize then deserialize again) instead of becoming unreadable.
    /// Acceptable since this crate never produces the variant itself -- it
    /// only exists transiently after reading a newer version's output.
    /// Excluded from the JSON Schema (`schemars(skip)`): it's not a shape
    /// this crate's own output can ever contain, so it has no business in
    /// the *published* schema.
    #[doc(hidden)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unrecognized,
}

/// Manual `Serialize` for `SqlText`. The
/// `#[derive(Serialize)]` this replaced serialized `Unrecognized` (a unit
/// variant) as the bare JSON string `"unrecognized"` -- valid for an
/// externally tagged enum, but a shape its own sibling `Deserialize` impl
/// (see below) can't read back: `visit_map` is never called for a bare
/// string, so re-reading a document containing `SqlText::Unrecognized`
/// failed deserialization entirely. The one property this forward-compat
/// mechanism *must* have -- surviving a read-then-write-then-read round
/// trip -- was silently broken (e.g. a cache or pipeline that stores
/// parsed output and reads it back would lose the whole document, not just
/// degrade the one field). Serializes every variant as the same
/// single-key-map shape `#[derive(Serialize)]` already produced for
/// `Variants`/`Union` (so existing fixtures/consumers see no change there),
/// plus `{"unrecognized": null}` for `Unrecognized` specifically.
impl Serialize for SqlText {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            SqlText::Variants(variants) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("variants", variants)?;
                map.end()
            }
            SqlText::Union { text, branch_count } => {
                #[derive(Serialize)]
                struct UnionRepr<'a> {
                    text: &'a SqlString,
                    branch_count: u32,
                }
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(
                    "union",
                    &UnionRepr {
                        text,
                        branch_count: *branch_count,
                    },
                )?;
                map.end()
            }
            SqlText::Unrecognized => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("unrecognized", &())?;
                map.end()
            }
        }
    }
}

/// Manual `Deserialize` for `SqlText` -- this can't
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
                let result = match key.as_str() {
                    "variants" => SqlText::Variants(map.next_value()?),
                    "union" => {
                        let union: UnionRepr = map.next_value()?;
                        SqlText::Union {
                            text: union.text,
                            branch_count: union.branch_count,
                        }
                    }
                    _ => {
                        // Forward-compat: some other SqlText shape this
                        // build doesn't recognize -- consume and discard
                        // its value rather than failing the whole document.
                        map.next_value::<serde::de::IgnoredAny>()?;
                        SqlText::Unrecognized
                    }
                };
                // B28 (cold code review): drain the MapAccess for a second
                // key before returning. Without this, a genuinely
                // malformed multi-key map (e.g. `{"variants": [...],
                // "extra": 1}`) left its second key/value unconsumed --
                // the *enclosing* deserializer would then choke on
                // leftover input with a confusing "trailing characters"/
                // unexpected-comma error instead of a clear message
                // naming the actual problem (more than one key where
                // exactly one was expected).
                if map.next_key::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::invalid_length(2, &"a single-key map"));
                }
                Ok(result)
            }
        }

        deserializer.deserialize_map(SqlTextVisitor)
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SqlVariant {
    pub text: SqlString,
    /// The `test` expressions (verbatim) that activate this variant --
    /// positive-only, and not a full boolean formula.
    ///
    /// Only the `test` conditions of `<if>`/dynamic tags whose branch is
    /// actually *taken* in this variant's path through the tag tree are
    /// recorded here, in document order. An `<if>`'s *not-taken* path
    /// contributes an alternative with `conditions: []` (empty) -- which
    /// is indistinguishable, at the type level, from a statement that had
    /// no `<if>` at all. This is by design (recording "this condition was
    /// false" would require inventing a negated-expression representation
    /// this crate doesn't have a use for elsewhere), but it means an empty
    /// `conditions` list is not itself proof that a variant is
    /// unconditional -- a consumer that needs that distinction has to
    /// correlate against the source XML's own dynamic-tag structure.
    ///
    /// `SqlText::Variants` as a whole lists *candidate* SQL shapes, not a
    /// runtime guarantee: this crate has no visibility into actual
    /// parameter values, so it cannot say which variant (if any single
    /// one) will execute for a given call -- only that these are the
    /// shapes the dynamic-tag tree can produce, gated by the conditions
    /// listed here.
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

/// The include-token textual contract -- a stable part of the v1 output.
///
/// Every `<include refid="...">` marker renders into the flattened
/// [`SqlText`] as a SQL block comment: an opening `/`+`*`, the literal
/// text `batis:include(`, this struct's own `raw` field, a closing `)`,
/// then the closing `*`+`/`. `raw` is rendered verbatim -- the unparsed
/// `refid` attribute value -- **except** any literal `*` immediately
/// followed by `/` inside it is rewritten to `*` + `_` + `/`, so the
/// token can never terminate its own enclosing comment early (a `refid`
/// is untrusted XML attribute content, not something this crate controls
/// the shape of). This holds regardless of `target`'s classification:
/// `Local("frag")` renders the comment around `frag` verbatim;
/// `Qualified { ns: "otherNs", id: "frag" }` renders the *original,
/// still-dotted* text `otherNs.frag` (`raw` is the whole unparsed
/// attribute value; `ns`/`id` are just it split on the last dot for
/// convenience, not a separate rendering); `Dynamic` renders the literal,
/// unresolved `${...}` text as-is.
///
/// **Locating tokens**: since the token's opening (`/`+`*` followed by
/// the literal text `batis:include(`) is a fixed prefix, a plain
/// substring search over the flattened SQL text finds every token
/// directly -- no need to reconstruct it from `raw` first. Each token's
/// position correlates 1:1 with one entry in the owning
/// [`Statement::includes`]/[`SqlFragment::includes`] list: match by
/// `Spanned::span`, which is the *original XML* span of the `<include>`
/// element (not a position in the flattened text) -- the same span a
/// [`DiagCode::IncludeAtWrapperBoundary`] diagnostic reports when this
/// token sits at a `<where>`/`<set>`/`<trim>` boundary (see that
/// variant's own doc comment, and the README's "Include expansion
/// order" section, for the substitution contract itself).
///
/// **Substituting a fragment with multiple variants**: a referenced
/// `<sql>` fragment is itself flattened to a [`SqlText`], which may be
/// `Variants` (several condition-gated alternatives) rather than one
/// fixed string. There is no single deterministic substitution in that
/// case -- the fragment's *own* active variant depends on the same
/// runtime parameter state as the enclosing statement's variant does, so
/// a consumer substituting fragment text into one variant of the parent
/// statement must pick the matching variant of the fragment (by
/// `conditions`), not an arbitrary one (e.g. `variants[0]`).
///
/// **Document order**: [`Mapper::statements`] (and `fragments`/
/// `result_maps`) preserve source document order -- safe to assume when
/// resolving forward/backward references across statements in one file.
///
/// **Diagnostic messages are not a stable matching surface.** `message`
/// strings may be reworded between versions without that being a
/// breaking change; match on [`Diagnostic::code`] instead.
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
/// ## Expansion-order contract
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
    fn b28_sql_text_multi_key_map_gets_a_clear_error_not_a_trailing_data_one() {
        // Cold code review B28: visit_map used to return as soon as it
        // consumed the first key's value, leaving a second key/value
        // unconsumed in the MapAccess -- serde_json would then choke on
        // the leftover input with a confusing "trailing characters"-style
        // error rather than a message naming the actual problem (more
        // than one key where exactly one was expected). This checks both
        // that deserialization fails *and* that the message names the
        // real cause instead of a generic trailing-data complaint.
        let json = r#"{"variants":[],"extra":1}"#;
        let err = serde_json::from_str::<SqlText>(json)
            .expect_err("a multi-key map must fail deserialization");
        let message = err.to_string();
        assert!(
            message.contains("single-key map"),
            "expected an error naming the single-key-map requirement, got: {message}"
        );
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
        assert_eq!(round_tripped, r#"{"unrecognized":null}"#);
    }

    #[test]
    fn sql_text_unrecognized_survives_a_full_serialize_deserialize_round_trip() {
        // A11 (cold code review, major): the whole point of a forward-compat
        // fallback is that it survives being written out and read back in
        // (e.g. a cache or pipeline storing this crate's own JSON output) --
        // the old bare-string `"unrecognized"` serialization broke exactly
        // that, since the deserializer only ever calls visit_map.
        let json = r#"{"totally_new_shape":42}"#;
        let sql: SqlText = serde_json::from_str(json).expect("first deserialize");
        let serialized = serde_json::to_string(&sql).expect("serialize");
        let round_tripped: SqlText =
            serde_json::from_str(&serialized).expect("second deserialize must not fail");
        assert_eq!(round_tripped, SqlText::Unrecognized);
    }

    #[test]
    fn sql_text_variants_and_union_serialize_shape_is_unchanged_by_the_manual_impl() {
        // The manual Serialize impl (A11) must reproduce the exact same
        // externally tagged shape #[derive(Serialize)] already produced for
        // these two variants -- only Unrecognized's shape should change.
        let variants = SqlText::Variants(vec![SqlVariant {
            text: SqlString {
                text: "SELECT 1".to_string(),
                span_map: vec![(0, 0)],
            },
            conditions: vec![],
        }]);
        assert_eq!(
            serde_json::to_string(&variants).unwrap(),
            r#"{"variants":[{"text":{"text":"SELECT 1","span_map":[[0,0]]},"conditions":[]}]}"#
        );

        let union = SqlText::Union {
            text: SqlString {
                text: "SELECT 1".to_string(),
                span_map: vec![(0, 0)],
            },
            branch_count: 33,
        };
        assert_eq!(
            serde_json::to_string(&union).unwrap(),
            r#"{"union":{"text":{"text":"SELECT 1","span_map":[[0,0]]},"branch_count":33}}"#
        );
    }
}

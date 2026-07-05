//! Dynamic-tag flattening (MM-06).
//!
//! MyBatis: `<if>`, `<choose>/<when>/<otherwise>`, `<where>`, `<set>`,
//! `<trim>`, `<foreach>`, `<bind>`. iBatis: `<dynamic>`, `<isNotEmpty>`,
//! `<isEqual>`, `<iterate>`, … (MM-11).
//!
//! Rule: if the branch combination count (cartesian product of tag
//! branches) is at most [`BRANCH_LIMIT`], emit `SqlText::Variants`;
//! otherwise fall back to `SqlText::Union` (all-branch union text) plus a
//! `BranchLimitExceeded` diagnostic.
//!
//! Every segment of the produced text maps back to original byte offsets
//! (`SqlString::span_map`) so downstream SQL analysis can point at the XML
//! source.
//!
//! ## MM-06a/06b/06c scope
//!
//! `<if>` and `<choose>/<when>/<otherwise>` have real branch semantics
//! (MM-06a). `<where>`/`<set>`/`<trim>`/`<foreach>`/`<bind>` have real
//! wrapper semantics (MM-06b). iBatis's `<isNotEmpty>`/`<isEqual>`/etc.
//! (any tag name starting with `is`), `<dynamic>`, and `<iterate>` have
//! real semantics too (MM-06c, see [`expand_conditional`]/
//! [`expand_dynamic`]/[`expand_iterate`]). Any *other* dynamic tag is still
//! a transparent passthrough — recursed into (so a nested `<if>` still
//! branches) but contributes no wrapper text or branch factor of its own.
//!
//! `<include>` markers found anywhere during this recursive descent
//! (top-level or nested) are collected into [`FlattenResult::found_includes`]
//! — `build_mapper` (parse.rs) merges these with `lift_includes`'s
//! top-level-only pass, deduping by span, to populate
//! `Statement.includes`/`SqlFragment.includes`.
//!
//! ## Why each text segment is normalized exactly once
//!
//! A shared body segment (e.g. text inside a `<choose>`'s `<when>`) ends up
//! cloned into every output `SqlVariant` that includes it. Placeholder
//! normalization (and its diagnostics/`property_paths`) must happen
//! *before* that cloning — once per physical segment, not once per output
//! variant — or a path/diagnostic would be reported once per variant that
//! happens to include it. [`Piece::Normalized`] stores the already-resolved
//! text, so [`assemble`] is pure concatenation with no further
//! normalization.

use crate::model::*;
use crate::parse::{
    attr_value_spanned, capture_subtree, classify_include, scan_attributes, BodySegment,
};
use crate::placeholder;

/// Per-statement cap on flattened candidates (fixed by spec).
pub(crate) const BRANCH_LIMIT: u32 = 32;

/// Recursion depth cap (fixed by spec) shared by every recursive descent
/// keyed per XML nesting level -- `flatten_segments`'s `expand_*` family,
/// `union_walk`, and (parse.rs) `collect_mappings`. Pathologically deep
/// nesting (thousands of levels) would otherwise reach this deep in the
/// *Rust call stack* before any branch-count check ever gets a chance to
/// bail out (each level's cartesian multiplication only happens on the
/// way back up, after the full depth-first descent) -- a stack overflow
/// aborts the process (uncatchable), unlike every other anomaly in this
/// crate. Cold code review B2/B3, 2026-07-05.
pub(crate) const DEPTH_LIMIT: u32 = 256;

pub(crate) struct FlattenResult {
    pub(crate) sql: SqlText,
    pub(crate) property_paths: Vec<Spanned<String>>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// `<include>` markers found anywhere in the body (top-level and
    /// nested inside dynamic tags). A missing `refid` is *not* diagnosed
    /// here — `lift_includes`'s top-level pass already covers that for
    /// top-level markers; a nested `<include>` missing `refid` is a known,
    /// minor gap (documented, not diagnosed twice).
    pub(crate) found_includes: Vec<Spanned<IncludeRef>>,
}

/// One ordered, already-normalized unit of assembled text for a single
/// branch alternative.
#[derive(Clone)]
enum Piece {
    Normalized {
        text: String,
        span_map: Vec<(u32, u32)>,
    },
    Include {
        raw: String,
        span: ByteSpan,
    },
    /// An iBatis conditional tag's own `prepend` attribute, rendered as
    /// `"{value} "` before its body. Kept as a distinct variant (rather
    /// than baked into a `Normalized` piece) so `<dynamic>`'s
    /// `removeFirstPrepend` behavior can find and strip *specifically* the
    /// first rendered child's prepend, instead of guessing from text
    /// patterns (a `prepend` value isn't necessarily "AND"/"OR").
    Prepend {
        value: String,
        span: ByteSpan,
    },
}

/// One branch alternative: an ordered piece sequence plus the `test`
/// expressions (document order) that activate it.
#[derive(Clone)]
struct Alt {
    pieces: Vec<Piece>,
    conditions: Vec<String>,
}

fn empty_alt() -> Alt {
    Alt {
        pieces: Vec::new(),
        conditions: Vec::new(),
    }
}

/// Mutable side-channels threaded through the whole recursive descent, so
/// `flatten_segments` and the per-tag `expand_*` helpers don't grow an
/// ever-longer parameter list as more tag types gain diagnostics/paths.
struct Ctx {
    dialect: Dialect,
    diagnostics: Vec<Diagnostic>,
    property_paths: Vec<Spanned<String>>,
    found_includes: Vec<Spanned<IncludeRef>>,
    /// Current recursion depth (nesting levels of dynamic tags descended
    /// so far in this pass) -- see [`DEPTH_LIMIT`].
    depth: u32,
}

/// The diagnostic emitted once a recursive descent hits [`DEPTH_LIMIT`].
/// `span: None`, like `BranchLimitExceeded`: this is a whole-subtree
/// truncation, not a single point in the source.
fn nesting_limit_diagnostic() -> Diagnostic {
    Diagnostic {
        code: DiagCode::NestingLimitExceeded,
        span: None,
        message: format!(
            "dynamic-tag nesting exceeds the depth cap of {DEPTH_LIMIT}; remaining subtree treated as opaque (no text contribution)"
        ),
    }
}

/// Flattens a statement or fragment body into its final [`SqlText`],
/// gathering `property_paths` from every text segment visited (top-level
/// *and* nested inside dynamic tags — this supersedes MM-07's top-level-only
/// pass, since flattening is the first place that actually recurses into
/// `<if>`/`<choose>` bodies).
pub(crate) fn flatten_body(
    source: &str,
    dialect: Dialect,
    segments: &[BodySegment],
) -> FlattenResult {
    let mut attempt = Ctx {
        dialect,
        diagnostics: Vec::new(),
        property_paths: Vec::new(),
        found_includes: Vec::new(),
        depth: 0,
    };

    match flatten_segments(source, segments, &mut attempt) {
        Ok(alts) => {
            let variants = alts
                .into_iter()
                .map(|alt| SqlVariant {
                    text: assemble(&alt.pieces),
                    conditions: alt.conditions,
                })
                .collect();
            FlattenResult {
                sql: SqlText::Variants(variants),
                property_paths: attempt.property_paths,
                diagnostics: attempt.diagnostics,
                found_includes: attempt.found_includes,
            }
        }
        Err(branch_count) => {
            // The cartesian attempt above bailed out partway through, so
            // its diagnostics/property_paths/found_includes only cover
            // part of the tree — discard them and do a fresh
            // non-combinatorial pass instead.
            let mut ctx = Ctx {
                dialect,
                diagnostics: Vec::new(),
                property_paths: Vec::new(),
                found_includes: Vec::new(),
                depth: 0,
            };
            let pieces = union_walk(source, segments, &mut ctx);
            let text = assemble(&pieces);
            ctx.diagnostics.push(Diagnostic {
                code: DiagCode::BranchLimitExceeded,
                span: None, // whole-body scope; no single span represents it
                message: format!(
                    "branch combinations ({branch_count}+) exceed the cap of {BRANCH_LIMIT}; falling back to a union of all branch text"
                ),
            });
            FlattenResult {
                sql: SqlText::Union {
                    text,
                    branch_count: branch_count.min(u32::MAX as u64) as u32,
                },
                property_paths: ctx.property_paths,
                diagnostics: ctx.diagnostics,
                found_includes: ctx.found_includes,
            }
        }
    }
}

/// Recursively expands `segments` into every branch alternative. `Err(n)`
/// means the count exceeded [`BRANCH_LIMIT`] somewhere in this subtree (`n`
/// is a valid lower bound, not necessarily the exact total) — the caller
/// aborts cartesian expansion and falls back to [`union_walk`].
///
/// Depth-limited (see [`DEPTH_LIMIT`]): a thin wrapper around
/// `flatten_segments_inner` that checks/increments/decrements `ctx.depth`
/// around every recursive descent, so the check applies uniformly
/// regardless of which `expand_*` helper calls back in here.
fn flatten_segments(
    source: &str,
    segments: &[BodySegment],
    ctx: &mut Ctx,
) -> Result<Vec<Alt>, u64> {
    if ctx.depth >= DEPTH_LIMIT {
        ctx.diagnostics.push(nesting_limit_diagnostic());
        return Ok(vec![empty_alt()]);
    }
    ctx.depth += 1;
    let result = flatten_segments_inner(source, segments, ctx);
    ctx.depth -= 1;
    result
}

/// One dispatch unit for the main flattening/union walks: either a run of
/// one or more consecutive [`BodySegment::Text`] entries, or a single
/// [`BodySegment::DynamicTag`]. B16 (cold code review): a run must be
/// normalized as ONE logical piece of text (see [`normalize_run`]) so a
/// placeholder split across a text/CDATA boundary (two separate
/// `BodySegment::Text` entries in document order) is recognized as one
/// placeholder, not an unterminated one plus stray leftover text.
enum RunItem<'a> {
    Text(Vec<&'a crate::parse::TextSegment>),
    Tag {
        name: &'a String,
        span: &'a ByteSpan,
    },
}

/// Groups `segments` into [`RunItem`]s in document order.
fn group_runs(segments: &[BodySegment]) -> Vec<RunItem<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < segments.len() {
        match &segments[i] {
            BodySegment::Text(_) => {
                let mut run = Vec::new();
                while let Some(BodySegment::Text(t)) = segments.get(i) {
                    run.push(t);
                    i += 1;
                }
                out.push(RunItem::Text(run));
            }
            BodySegment::DynamicTag { name, span } => {
                out.push(RunItem::Tag { name, span });
                i += 1;
            }
        }
    }
    out
}

/// Normalizes a run of adjacent text segments as one logical piece
/// (B16), folding the result into `ctx` and returning the assembled
/// [`Piece`].
fn normalize_run(texts: &[&crate::parse::TextSegment], ctx: &mut Ctx) -> Piece {
    let runs: Vec<placeholder::TextRun> = texts
        .iter()
        .map(|t| placeholder::TextRun {
            decoded: &t.decoded,
            raw_span: t.raw_span,
        })
        .collect();
    let mut result = placeholder::normalize_merged(&runs, ctx.dialect);
    ctx.property_paths.append(&mut result.property_paths);
    ctx.diagnostics.append(&mut result.diagnostics);
    Piece::Normalized {
        text: result.text,
        span_map: result.span_map,
    }
}

fn flatten_segments_inner(
    source: &str,
    segments: &[BodySegment],
    ctx: &mut Ctx,
) -> Result<Vec<Alt>, u64> {
    let mut acc = vec![empty_alt()];

    for item in group_runs(segments) {
        match item {
            RunItem::Text(texts) => {
                let piece = normalize_run(&texts, ctx);
                for alt in &mut acc {
                    alt.pieces.push(piece.clone());
                }
            }
            RunItem::Tag { name, span } => match name.as_str() {
                "include" => {
                    record_include(source, *span, ctx);
                    let raw = read_refid(source, *span);
                    for alt in &mut acc {
                        alt.pieces.push(Piece::Include {
                            raw: raw.clone(),
                            span: *span,
                        });
                    }
                }
                "bind" => {
                    // Contributes no text; its expression lives in an
                    // attribute (`value`), never in a Text/CDATA event, so
                    // MM-07's normalization never sees it — record it here.
                    let value = read_attr(source, *span, b"value", ctx);
                    if !value.is_empty() {
                        ctx.property_paths.push(Spanned { value, span: *span });
                    }
                }
                "if" => {
                    let local = expand_if(source, *span, ctx)?;
                    acc = try_combine(&acc, &local)?;
                }
                "choose" => {
                    let local = expand_choose(source, *span, ctx)?;
                    acc = try_combine(&acc, &local)?;
                }
                "where" => {
                    let local = expand_where(source, *span, ctx)?;
                    acc = try_combine(&acc, &local)?;
                }
                "set" => {
                    let local = expand_set(source, *span, ctx)?;
                    acc = try_combine(&acc, &local)?;
                }
                "trim" => {
                    let local = expand_trim(source, *span, ctx)?;
                    acc = try_combine(&acc, &local)?;
                }
                "foreach" => {
                    let local = expand_foreach(source, *span, ctx)?;
                    acc = try_combine(&acc, &local)?;
                }
                "dynamic" => {
                    let local = expand_dynamic(source, *span, ctx)?;
                    acc = try_combine(&acc, &local)?;
                }
                "iterate" => {
                    let local = expand_iterate(source, *span, ctx)?;
                    acc = try_combine(&acc, &local)?;
                }
                _ if name.starts_with("is") => {
                    let local = expand_conditional(source, name, *span, ctx)?;
                    acc = try_combine(&acc, &local)?;
                }
                _ => {
                    // Transparent passthrough — anything with no special
                    // semantics at all.
                    let local = expand_transparent(source, *span, ctx)?;
                    acc = try_combine(&acc, &local)?;
                }
            },
        }
    }

    Ok(acc)
}

/// `<if test="...">body</if>` → 2 alternatives: absent (empty, no
/// condition) and present (body's own alternatives, each prefixed with
/// `test`).
fn expand_if(source: &str, span: ByteSpan, ctx: &mut Ctx) -> Result<Vec<Alt>, u64> {
    let test_value = read_attr(source, span, b"test", ctx);

    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    ctx.diagnostics.append(&mut inner_diags);
    let inner_alts = flatten_segments(source, &inner_segments, ctx)?;

    let mut local = vec![empty_alt()];
    for alt in inner_alts {
        let mut conditions = vec![test_value.clone()];
        conditions.extend(alt.conditions);
        local.push(Alt {
            pieces: alt.pieces,
            conditions,
        });
    }

    if local.len() as u64 > BRANCH_LIMIT as u64 {
        return Err(local.len() as u64);
    }
    Ok(local)
}

/// `<choose>` with N `<when>` + optional `<otherwise>` → N+1 alternatives
/// (without `<otherwise>`, one alternative is empty).
fn expand_choose(source: &str, span: ByteSpan, ctx: &mut Ctx) -> Result<Vec<Alt>, u64> {
    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    ctx.diagnostics.append(&mut inner_diags);

    let mut local = Vec::new();
    let mut has_otherwise = false;

    for segment in &inner_segments {
        let BodySegment::DynamicTag {
            name,
            span: child_span,
        } = segment
        else {
            continue; // stray text between <when>/<otherwise> — no branch content
        };

        if name == "when" {
            let test_value = read_attr(source, *child_span, b"test", ctx);
            let (when_segments, mut when_diags, _t) = capture_subtree(source, *child_span);
            ctx.diagnostics.append(&mut when_diags);
            let when_alts = flatten_segments(source, &when_segments, ctx)?;
            for alt in when_alts {
                let mut conditions = vec![test_value.clone()];
                conditions.extend(alt.conditions);
                local.push(Alt {
                    pieces: alt.pieces,
                    conditions,
                });
            }
        } else if name == "otherwise" {
            has_otherwise = true;
            let (otherwise_segments, mut o_diags, _t) = capture_subtree(source, *child_span);
            ctx.diagnostics.append(&mut o_diags);
            local.extend(flatten_segments(source, &otherwise_segments, ctx)?);
        } else {
            // Cold code review B7: anything else (including a stray
            // <include>) used to vanish silently here -- no text
            // contribution, no diagnostic, nothing telling a consumer
            // it was even there.
            ctx.diagnostics.push(Diagnostic {
                code: DiagCode::UnknownElement,
                span: Some(*child_span),
                message: format!(
                    "<choose> child <{name}> is neither <when> nor <otherwise> -- ignored"
                ),
            });
        }

        if local.len() as u64 > BRANCH_LIMIT as u64 {
            return Err(local.len() as u64);
        }
    }

    if !has_otherwise {
        local.push(empty_alt());
    }
    Ok(local)
}

/// Any dynamic tag with no special semantics yet: recurse into its body
/// and use that body's own alternatives as-is, with no wrapper text and no
/// branch factor contributed by the container itself.
fn expand_transparent(source: &str, span: ByteSpan, ctx: &mut Ctx) -> Result<Vec<Alt>, u64> {
    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    ctx.diagnostics.append(&mut inner_diags);
    flatten_segments(source, &inner_segments, ctx)
}

/// `<where>`: for each inner-body alternative, if its assembled text is
/// empty/whitespace-only, the wrapper contributes nothing (no `WHERE` at
/// all); otherwise strip one leading `AND`/`OR` (case-insensitive, word
/// boundary) and prepend `WHERE `.
fn expand_where(source: &str, span: ByteSpan, ctx: &mut Ctx) -> Result<Vec<Alt>, u64> {
    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    ctx.diagnostics.append(&mut inner_diags);
    let inner_alts = flatten_segments(source, &inner_segments, ctx)?;

    let local = inner_alts
        .into_iter()
        .map(|alt| {
            let inner_sql = assemble(&alt.pieces);
            let pieces = if inner_sql.text.trim().is_empty() {
                Vec::new()
            } else {
                let strip_n = leading_and_or_strip_len(&inner_sql.text);
                vec![to_piece(with_prefix(
                    inner_sql, span.start, strip_n, "WHERE ",
                ))]
            };
            Alt {
                pieces,
                conditions: alt.conditions,
            }
        })
        .collect();
    Ok(local)
}

/// `<set>`: same empty-body suppression as `<where>`; otherwise prepend
/// `SET ` and strip one trailing comma (and the whitespace after it).
fn expand_set(source: &str, span: ByteSpan, ctx: &mut Ctx) -> Result<Vec<Alt>, u64> {
    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    ctx.diagnostics.append(&mut inner_diags);
    let inner_alts = flatten_segments(source, &inner_segments, ctx)?;

    let local = inner_alts
        .into_iter()
        .map(|alt| {
            let inner_sql = assemble(&alt.pieces);
            let pieces = if inner_sql.text.trim().is_empty() {
                Vec::new()
            } else {
                let trailing_strip = trailing_comma_strip_len(&inner_sql.text);
                let stripped = with_suffix_strip(inner_sql, trailing_strip);
                vec![to_piece(with_prefix(stripped, span.start, 0, "SET "))]
            };
            Alt {
                pieces,
                conditions: alt.conditions,
            }
        })
        .collect();
    Ok(local)
}

/// `<trim prefix suffix prefixOverrides suffixOverrides>`: prefix/suffix
/// are added only when the inner text is non-empty; `*Overrides` are
/// pipe-separated alternative lists (`"AND |OR "`), of which at most one
/// match is stripped from each side.
fn expand_trim(source: &str, span: ByteSpan, ctx: &mut Ctx) -> Result<Vec<Alt>, u64> {
    let prefix = read_attr(source, span, b"prefix", ctx);
    let suffix = read_attr(source, span, b"suffix", ctx);
    let prefix_overrides = split_overrides(&read_attr(source, span, b"prefixOverrides", ctx));
    let suffix_overrides = split_overrides(&read_attr(source, span, b"suffixOverrides", ctx));

    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    ctx.diagnostics.append(&mut inner_diags);
    let inner_alts = flatten_segments(source, &inner_segments, ctx)?;

    let local = inner_alts
        .into_iter()
        .map(|alt| {
            let inner_sql = assemble(&alt.pieces);
            let pieces = if inner_sql.text.trim().is_empty() {
                Vec::new()
            } else {
                let lead_strip = leading_override_strip_len(&inner_sql.text, &prefix_overrides);
                let trail_strip = trailing_override_strip_len(&inner_sql.text, &suffix_overrides);
                let with_lead = with_prefix(inner_sql, span.start, lead_strip, &prefix);
                let trimmed = with_suffix_strip(with_lead, trail_strip);
                vec![to_piece(with_suffix(trimmed, span.start, &suffix))]
            };
            Alt {
                pieces,
                conditions: alt.conditions,
            }
        })
        .collect();
    Ok(local)
}

/// `<foreach open close separator>`: a repetition, not a branch — each
/// inner alternative is rendered once, wrapped with `open`/`close` (no
/// stripping); `separator` describes repeated-item joining at runtime and
/// is not representable statically, so it's ignored here.
fn expand_foreach(source: &str, span: ByteSpan, ctx: &mut Ctx) -> Result<Vec<Alt>, u64> {
    let open = read_attr(source, span, b"open", ctx);
    let close = read_attr(source, span, b"close", ctx);

    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    ctx.diagnostics.append(&mut inner_diags);
    let inner_alts = flatten_segments(source, &inner_segments, ctx)?;

    let local = inner_alts
        .into_iter()
        .map(|alt| {
            let inner_sql = assemble(&alt.pieces);
            let wrapped = with_suffix(
                with_prefix(inner_sql, span.start, 0, &open),
                span.start,
                &close,
            );
            Alt {
                pieces: vec![to_piece(wrapped)],
                conditions: alt.conditions,
            }
        })
        .collect();
    Ok(local)
}

/// iBatis conditional tags (`<isNotEmpty>`, `<isEqual>`, ... — anything
/// named `is*`, known or not) → 2 alternatives: absent (empty, no
/// condition) and present (body's own alternatives, each prefixed with a
/// synthesized condition and, if the tag has a `prepend` attribute, that
/// value rendered as a leading [`Piece::Prepend`]).
fn expand_conditional(
    source: &str,
    tag_name: &str,
    span: ByteSpan,
    ctx: &mut Ctx,
) -> Result<Vec<Alt>, u64> {
    let condition = synthesize_condition(tag_name, source, span, ctx);
    let prepend = read_attr_opt(source, span, b"prepend", ctx);

    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    ctx.diagnostics.append(&mut inner_diags);
    let inner_alts = flatten_segments(source, &inner_segments, ctx)?;

    let mut local = vec![empty_alt()];
    for alt in inner_alts {
        let mut pieces = Vec::new();
        if let Some(value) = &prepend {
            pieces.push(Piece::Prepend {
                value: value.clone(),
                span,
            });
        }
        pieces.extend(alt.pieces);
        let mut conditions = vec![condition.clone()];
        conditions.extend(alt.conditions);
        local.push(Alt { pieces, conditions });
    }

    if local.len() as u64 > BRANCH_LIMIT as u64 {
        return Err(local.len() as u64);
    }
    Ok(local)
}

/// Synthesizes a condition string from an iBatis conditional tag's
/// semantics, e.g. `isNotEmpty(grpCd)` or `isEqual(status, 'Y')`. Unknown
/// `is*` tag names are handled exactly the same way — safer than silently
/// treating an unrecognized conditional as a non-branching passthrough.
fn synthesize_condition(tag_name: &str, source: &str, span: ByteSpan, ctx: &mut Ctx) -> String {
    let property = read_attr(source, span, b"property", ctx);
    if let Some(value) = read_attr_opt(source, span, b"compareValue", ctx) {
        format!("{tag_name}({property}, '{value}')")
    } else if let Some(prop) = read_attr_opt(source, span, b"compareProperty", ctx) {
        format!("{tag_name}({property}, {prop})")
    } else {
        format!("{tag_name}({property})")
    }
}

/// `<dynamic prepend="WHERE">`: like `<where>`, contributes its `prepend`
/// only when the assembled inner text is non-empty — but it also
/// suppresses the *first rendered* child's own prepend (iBatis's
/// `removeFirstPrepend`), since otherwise two `<isNotEmpty prepend="AND">`
/// children would render `WHERE AND a = ? AND b = ?`. "First rendered"
/// varies by branch combination, so this operates per-variant, after the
/// inner alternatives are known but before their pieces are assembled to
/// text (stripping a specific piece is precise; guessing from rendered
/// text — like `<where>`'s AND/OR scan — isn't, since `prepend` can be any
/// string).
fn expand_dynamic(source: &str, span: ByteSpan, ctx: &mut Ctx) -> Result<Vec<Alt>, u64> {
    let prepend = read_attr(source, span, b"prepend", ctx);

    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    ctx.diagnostics.append(&mut inner_diags);
    let inner_alts = flatten_segments(source, &inner_segments, ctx)?;

    let local = inner_alts
        .into_iter()
        .map(|mut alt| {
            remove_first_prepend(&mut alt.pieces);
            let inner_sql = assemble(&alt.pieces);
            let pieces = if inner_sql.text.trim().is_empty() {
                Vec::new()
            } else if prepend.is_empty() {
                vec![to_piece(inner_sql)]
            } else {
                vec![to_piece(with_prefix(
                    inner_sql,
                    span.start,
                    0,
                    &format!("{prepend} "),
                ))]
            };
            Alt {
                pieces,
                conditions: alt.conditions,
            }
        })
        .collect();
    Ok(local)
}

/// Removes the first [`Piece::Prepend`] found (if any) — see
/// [`expand_dynamic`].
fn remove_first_prepend(pieces: &mut Vec<Piece>) {
    if let Some(pos) = pieces
        .iter()
        .position(|p| matches!(p, Piece::Prepend { .. }))
    {
        pieces.remove(pos);
    }
}

/// `<iterate property open close conjunction>`: mirrors `<foreach>` — a
/// repetition, not a branch. `conjunction` (iterate's analog of
/// `foreach`'s `separator`) describes runtime item-joining and isn't
/// representable statically, so it's ignored. `property` names the
/// iterated collection; like `<bind>`'s `value`, it lives in an attribute
/// that MM-07 normalization never sees, so it's recorded explicitly.
fn expand_iterate(source: &str, span: ByteSpan, ctx: &mut Ctx) -> Result<Vec<Alt>, u64> {
    let open = read_attr(source, span, b"open", ctx);
    let close = read_attr(source, span, b"close", ctx);
    if let Some(property) = read_attr_opt(source, span, b"property", ctx) {
        if !property.is_empty() {
            ctx.property_paths.push(Spanned {
                value: property,
                span,
            });
        }
    }

    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    ctx.diagnostics.append(&mut inner_diags);
    let inner_alts = flatten_segments(source, &inner_segments, ctx)?;

    let local = inner_alts
        .into_iter()
        .map(|alt| {
            let inner_sql = assemble(&alt.pieces);
            let wrapped = with_suffix(
                with_prefix(inner_sql, span.start, 0, &open),
                span.start,
                &close,
            );
            Alt {
                pieces: vec![to_piece(wrapped)],
                conditions: alt.conditions,
            }
        })
        .collect();
    Ok(local)
}

/// Cartesian-combines two alternative lists, bailing out with the (not
/// materialized) product count if it would exceed [`BRANCH_LIMIT`].
fn try_combine(acc: &[Alt], local: &[Alt]) -> Result<Vec<Alt>, u64> {
    let product = acc.len() as u64 * local.len() as u64;
    if product > BRANCH_LIMIT as u64 {
        return Err(product);
    }
    let mut result = Vec::with_capacity(product as usize);
    for a in acc {
        for l in local {
            let mut pieces = a.pieces.clone();
            pieces.extend(l.pieces.clone());
            let mut conditions = a.conditions.clone();
            conditions.extend(l.conditions.clone());
            result.push(Alt { pieces, conditions });
        }
    }
    Ok(result)
}

/// Over-cap fallback: concatenates every branch's content once (no
/// cartesian multiplication) in document order — "each branch's content
/// concatenated once" rather than a syntactically valid query. Structurally
/// a single linear walk, so (unlike the cartesian path) each segment is
/// visited exactly once here regardless of tree shape — safe to normalize
/// directly. Wrapper tags are treated the same as transparent containers
/// here (their prefix/suffix semantics only make sense per-branch, and
/// there are no branches in a union).
fn union_walk(source: &str, segments: &[BodySegment], ctx: &mut Ctx) -> Vec<Piece> {
    // Depth-limited (see DEPTH_LIMIT): this walk is structurally separate
    // from flatten_segments (invoked once, on the whole original tree,
    // after the cartesian attempt above already bailed) and has its own
    // unbounded recursion below -- it needs the same guard.
    if ctx.depth >= DEPTH_LIMIT {
        ctx.diagnostics.push(nesting_limit_diagnostic());
        return Vec::new();
    }
    ctx.depth += 1;

    let mut pieces = Vec::new();

    for item in group_runs(segments) {
        match item {
            RunItem::Text(texts) => {
                pieces.push(normalize_run(&texts, ctx));
            }
            RunItem::Tag { name, span } if name == "include" => {
                record_include(source, *span, ctx);
                pieces.push(Piece::Include {
                    raw: read_refid(source, *span),
                    span: *span,
                });
            }
            RunItem::Tag { name, span } if name == "bind" => {
                let value = read_attr(source, *span, b"value", ctx);
                if !value.is_empty() {
                    ctx.property_paths.push(Spanned { value, span: *span });
                }
            }
            RunItem::Tag { name, span } if name == "iterate" => {
                if let Some(property) = read_attr_opt(source, *span, b"property", ctx) {
                    if !property.is_empty() {
                        ctx.property_paths.push(Spanned {
                            value: property,
                            span: *span,
                        });
                    }
                }
                let (inner_segments, mut d, _t) = capture_subtree(source, *span);
                ctx.diagnostics.append(&mut d);
                pieces.extend(union_walk(source, &inner_segments, ctx));
            }
            RunItem::Tag { name, span } if name == "choose" => {
                let (inner_segments, mut d, _t) = capture_subtree(source, *span);
                ctx.diagnostics.append(&mut d);
                for child in &inner_segments {
                    if let BodySegment::DynamicTag {
                        name: child_name,
                        span: child_span,
                    } = child
                    {
                        if child_name == "when" || child_name == "otherwise" {
                            let (child_segments, mut cd, _t) = capture_subtree(source, *child_span);
                            ctx.diagnostics.append(&mut cd);
                            pieces.extend(union_walk(source, &child_segments, ctx));
                        }
                    }
                }
            }
            RunItem::Tag { span, .. } => {
                let (inner_segments, mut d, _t) = capture_subtree(source, *span);
                ctx.diagnostics.append(&mut d);
                pieces.extend(union_walk(source, &inner_segments, ctx));
            }
        }
    }

    ctx.depth -= 1;
    pieces
}

/// Concatenates already-normalized pieces into one [`SqlString`],
/// shift-appending each piece's `span_map` and rendering each `<include>`
/// marker as a SQL-comment-safe token (silently dropping it would produce
/// misleading SQL — a consumer expands it later).
fn assemble(pieces: &[Piece]) -> SqlString {
    let mut text = String::new();
    let mut span_map = Vec::new();

    for piece in pieces {
        match piece {
            Piece::Normalized {
                text: t,
                span_map: sm,
            } => {
                let base = text.len() as u32;
                for (off, raw) in sm {
                    push_span_entry(&mut span_map, base + off, *raw);
                }
                text.push_str(t);
            }
            Piece::Include { raw, span } => {
                push_span_entry(&mut span_map, text.len() as u32, span.start);
                // Cold code review B15: a refid containing "*/" (however
                // contrived) would otherwise terminate this SQL comment
                // early, corrupting the rest of the rendered text. `raw`
                // is untrusted XML attribute content, not something this
                // crate controls the shape of.
                text.push_str(&format!(
                    "/* batis:include({}) */",
                    raw.replace("*/", "*_/")
                ));
            }
            Piece::Prepend { value, span } => {
                push_span_entry(&mut span_map, text.len() as u32, span.start);
                text.push_str(&format!("{value} "));
            }
        }
    }

    SqlString { text, span_map }
}

/// Pushes `(offset, raw)`, replacing rather than duplicating an existing
/// entry at the same offset — a piece's own leading entry can land on the
/// exact offset the previous piece's trailing entry already used (e.g. the
/// previous piece ends right on a placeholder replacement, with no
/// verbatim tail). The newer entry describes what actually starts at that
/// position, so it wins, keeping offsets strictly increasing.
fn push_span_entry(span_map: &mut Vec<(u32, u32)>, offset: u32, raw: u32) {
    if span_map.last().map(|(last_off, _)| *last_off) == Some(offset) {
        span_map.pop();
    }
    span_map.push((offset, raw));
}

fn to_piece(sql: SqlString) -> Piece {
    Piece::Normalized {
        text: sql.text,
        span_map: sql.span_map,
    }
}

/// Prepends `prefix` (synthetic text — no original bytes, so its span_map
/// entry points at the wrapper tag's own span start, same convention as
/// the `<include>` token) to `sql`, first stripping `strip_n` leading bytes
/// from `sql.text`. Re-bases the remaining span_map entries so offsets
/// stay correct and strictly increasing.
fn with_prefix(sql: SqlString, wrapper_start: u32, strip_n: usize, prefix: &str) -> SqlString {
    let kept = &sql.text[strip_n..];
    let mut span_map = vec![(0u32, wrapper_start)];

    if !prefix.is_empty() || strip_n > 0 {
        // Find the raw offset corresponding to position `strip_n` in the
        // original text: the last span_map entry at or before `strip_n`,
        // extrapolated by the byte delta (an honest approximation — see
        // placeholder.rs's span-fidelity note; exact when the run between
        // entries is verbatim, which it usually is for plain wrapper
        // boundary text like "AND ").
        let mut base_off = 0u32;
        let mut base_raw = sql
            .span_map
            .first()
            .map(|(_, r)| *r)
            .unwrap_or(wrapper_start);
        for (off, raw) in &sql.span_map {
            if (*off as usize) <= strip_n {
                base_off = *off;
                base_raw = *raw;
            } else {
                break;
            }
        }
        let split_raw = base_raw + (strip_n as u32 - base_off);
        push_span_entry(&mut span_map, prefix.len() as u32, split_raw);
    }

    for (off, raw) in &sql.span_map {
        if (*off as usize) > strip_n {
            push_span_entry(
                &mut span_map,
                prefix.len() as u32 + (off - strip_n as u32),
                *raw,
            );
        }
    }

    SqlString {
        text: format!("{prefix}{kept}"),
        span_map,
    }
}

/// Appends `suffix` (synthetic — spans at the wrapper's own start) to
/// `sql`. The kept (leading) portion of the text is unaffected, so its
/// existing span_map entries stay valid as-is.
fn with_suffix(sql: SqlString, wrapper_start: u32, suffix: &str) -> SqlString {
    let mut span_map = sql.span_map;
    if !suffix.is_empty() {
        push_span_entry(&mut span_map, sql.text.len() as u32, wrapper_start);
    }
    SqlString {
        text: format!("{}{suffix}", sql.text),
        span_map,
    }
}

/// Strips `strip_n` trailing bytes from `sql.text`, dropping any span_map
/// entries that would now point past the end of the (shortened) text.
fn with_suffix_strip(sql: SqlString, strip_n: usize) -> SqlString {
    if strip_n == 0 {
        return sql;
    }
    let keep_len = sql.text.len() - strip_n;
    let text = sql.text[..keep_len].to_string();
    let span_map = sql
        .span_map
        .into_iter()
        // Cold code review B9: an entry at exactly `keep_len` describes a
        // segment whose start now coincides with the truncated text's own
        // end -- zero surviving characters (everything from that offset
        // onward was just stripped), so it's a phantom one-past-end
        // entry, not a real remaining segment. `<`, not `<=`.
        .filter(|(off, _)| (*off as usize) < keep_len)
        .collect();
    SqlString { text, span_map }
}

/// Length of a leading `\s*(AND|OR)\s+` match (case-insensitive), or 0 if
/// the text doesn't start with one after skipping leading whitespace.
fn leading_and_or_strip_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let rest = &text[i..];
    for word in ["and", "or"] {
        // Byte-slice comparison: `rest[..word.len()]` (a &str index) panics
        // if that byte offset isn't a char boundary (e.g. a 3-byte CJK or
        // 4-byte emoji character right after the whitespace). Byte slices
        // have no such requirement, and since `word` is all-ASCII the
        // comparison semantics are identical either way.
        if rest.len() >= word.len()
            && rest.as_bytes()[..word.len()].eq_ignore_ascii_case(word.as_bytes())
        {
            let after = rest.as_bytes().get(word.len());
            if after.is_some_and(|b| b.is_ascii_whitespace()) {
                let mut j = i + word.len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                return j;
            }
        }
    }
    0
}

/// Length to strip from the end for `<set>`'s trailing-comma rule: a comma
/// immediately before the trailing whitespace run, plus that whitespace.
fn trailing_comma_strip_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut ws = 0;
    while ws < bytes.len() && bytes[bytes.len() - 1 - ws].is_ascii_whitespace() {
        ws += 1;
    }
    if bytes.len() > ws && bytes[bytes.len() - 1 - ws] == b',' {
        ws + 1
    } else {
        0
    }
}

fn split_overrides(raw: &str) -> Vec<String> {
    raw.split('|')
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Length of a leading match (after skipping leading whitespace) against
/// any of `overrides`, case-insensitively — at most one is stripped.
fn leading_override_strip_len(text: &str, overrides: &[String]) -> usize {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let rest = &text[i..];
    for candidate in overrides {
        // See leading_and_or_strip_len's comment: byte-slice comparison
        // never char-boundary-panics, and a byte-identical match
        // guarantees the boundary is valid anyway (candidate is a &str).
        if rest.len() >= candidate.len()
            && rest.as_bytes()[..candidate.len()].eq_ignore_ascii_case(candidate.as_bytes())
        {
            return i + candidate.len();
        }
    }
    0
}

/// Length of a trailing match (before trailing whitespace) against any of
/// `overrides`, case-insensitively — at most one is stripped.
fn trailing_override_strip_len(text: &str, overrides: &[String]) -> usize {
    let bytes = text.as_bytes();
    let mut ws = 0;
    while ws < bytes.len() && bytes[bytes.len() - 1 - ws].is_ascii_whitespace() {
        ws += 1;
    }
    let before_ws = &text[..text.len() - ws];
    for candidate in overrides {
        // See leading_and_or_strip_len's comment: byte-slice comparison
        // never char-boundary-panics, and a byte-identical match
        // guarantees the boundary is valid anyway (candidate is a &str).
        if before_ws.len() >= candidate.len()
            && before_ws.as_bytes()[before_ws.len() - candidate.len()..]
                .eq_ignore_ascii_case(candidate.as_bytes())
        {
            return ws + candidate.len();
        }
    }
    0
}

/// Reads and lifts an `<include>` marker into
/// [`FlattenResult::found_includes`]. A missing `refid` is silently
/// skipped here (see [`FlattenResult::found_includes`]'s doc comment).
fn record_include(source: &str, span: ByteSpan, ctx: &mut Ctx) {
    let attrs = scan_attributes(source.as_bytes(), span.start as usize, span.end as usize);
    let (refid, mut diags) = attr_value_spanned(source, &attrs, b"refid");
    ctx.diagnostics.append(&mut diags);
    if let Some(refid) = refid {
        let target = classify_include(&refid.value, ctx.dialect);
        ctx.found_includes.push(Spanned {
            value: IncludeRef {
                raw: refid.value,
                target,
            },
            span,
        });
    }
}

fn read_attr(source: &str, span: ByteSpan, name: &[u8], ctx: &mut Ctx) -> String {
    read_attr_opt(source, span, name, ctx).unwrap_or_default()
}

fn read_attr_opt(source: &str, span: ByteSpan, name: &[u8], ctx: &mut Ctx) -> Option<String> {
    let attrs = scan_attributes(source.as_bytes(), span.start as usize, span.end as usize);
    let (value, mut diags) = attr_value_spanned(source, &attrs, name);
    ctx.diagnostics.append(&mut diags);
    value.map(|v| v.value)
}

fn read_refid(source: &str, span: ByteSpan) -> String {
    let attrs = scan_attributes(source.as_bytes(), span.start as usize, span.end as usize);
    let (value, _diags) = attr_value_spanned(source, &attrs, b"refid");
    value.map(|v| v.value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    // mm_06_* end-to-end tests live in parse.rs (they exercise the full
    // parse_str pipeline); this module has no flatten-internal-only tests
    // yet since Piece/Alt aren't exposed for direct construction.
}

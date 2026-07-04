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
//! ## MM-06a scope
//!
//! `<if>` and `<choose>/<when>/<otherwise>` get real branch semantics.
//! Every other dynamic tag (`<where>`, `<set>`, `<trim>`, `<foreach>`,
//! `<bind>`, and all iBatis tags) is a *transparent passthrough* for now —
//! its body is recursed into (so nested `<if>` inside e.g. `<where>` still
//! branches correctly) but the container itself contributes no wrapper
//! text and no branch factor of its own. MM-06b (MyBatis wrappers) and
//! MM-06c (iBatis tags) replace this passthrough with real semantics.
//!
//! Known limitation: `Statement.includes`/`SqlFragment.includes` (MM-05)
//! only lift `<include>` markers found at the *top level* of a body — one
//! nested inside `<if>`/`<choose>`/etc. is invisible there (though its
//! text still renders correctly here, as an `/* atlas:include(id) */`
//! token). Extending MM-05's lift to recurse is not in this slice's scope.
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
use crate::parse::{attr_value_spanned, capture_subtree, scan_attributes, BodySegment};
use crate::placeholder;

/// Per-statement cap on flattened candidates (fixed by spec).
pub(crate) const BRANCH_LIMIT: u32 = 32;

pub(crate) struct FlattenResult {
    pub(crate) sql: SqlText,
    pub(crate) property_paths: Vec<Spanned<String>>,
    pub(crate) diagnostics: Vec<Diagnostic>,
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
    let mut attempt_diagnostics = Vec::new();
    let mut attempt_paths = Vec::new();

    match flatten_segments(
        source,
        dialect,
        segments,
        &mut attempt_diagnostics,
        &mut attempt_paths,
    ) {
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
                property_paths: attempt_paths,
                diagnostics: attempt_diagnostics,
            }
        }
        Err(branch_count) => {
            // The cartesian attempt above bailed out partway through, so
            // its diagnostics/property_paths only cover part of the tree —
            // discard them and do a fresh non-combinatorial pass instead.
            let mut diagnostics = Vec::new();
            let mut property_paths = Vec::new();
            let pieces = union_walk(
                source,
                dialect,
                segments,
                &mut diagnostics,
                &mut property_paths,
            );
            let text = assemble(&pieces);
            diagnostics.push(Diagnostic {
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
                property_paths,
                diagnostics,
            }
        }
    }
}

/// Recursively expands `segments` into every branch alternative. `Err(n)`
/// means the count exceeded [`BRANCH_LIMIT`] somewhere in this subtree (`n`
/// is a valid lower bound, not necessarily the exact total) — the caller
/// aborts cartesian expansion and falls back to [`union_walk`].
fn flatten_segments(
    source: &str,
    dialect: Dialect,
    segments: &[BodySegment],
    diagnostics: &mut Vec<Diagnostic>,
    property_paths: &mut Vec<Spanned<String>>,
) -> Result<Vec<Alt>, u64> {
    let mut acc = vec![empty_alt()];

    for segment in segments {
        match segment {
            BodySegment::Text(text) => {
                let mut result =
                    placeholder::normalize_segment(&text.decoded, text.raw_span, dialect);
                property_paths.append(&mut result.property_paths);
                diagnostics.append(&mut result.diagnostics);
                let piece = Piece::Normalized {
                    text: result.text,
                    span_map: result.span_map,
                };
                for alt in &mut acc {
                    alt.pieces.push(piece.clone());
                }
            }
            BodySegment::DynamicTag { name, span } if name == "include" => {
                let raw = read_refid(source, *span);
                for alt in &mut acc {
                    alt.pieces.push(Piece::Include {
                        raw: raw.clone(),
                        span: *span,
                    });
                }
            }
            BodySegment::DynamicTag { name, span } if name == "if" => {
                let local = expand_if(source, dialect, *span, diagnostics, property_paths)?;
                acc = try_combine(&acc, &local)?;
            }
            BodySegment::DynamicTag { name, span } if name == "choose" => {
                let local = expand_choose(source, dialect, *span, diagnostics, property_paths)?;
                acc = try_combine(&acc, &local)?;
            }
            BodySegment::DynamicTag { span, .. } => {
                // Transparent passthrough — see module docs.
                let local =
                    expand_transparent(source, dialect, *span, diagnostics, property_paths)?;
                acc = try_combine(&acc, &local)?;
            }
        }
    }

    Ok(acc)
}

/// `<if test="...">body</if>` → 2 alternatives: absent (empty, no
/// condition) and present (body's own alternatives, each prefixed with
/// `test`).
fn expand_if(
    source: &str,
    dialect: Dialect,
    span: ByteSpan,
    diagnostics: &mut Vec<Diagnostic>,
    property_paths: &mut Vec<Spanned<String>>,
) -> Result<Vec<Alt>, u64> {
    let test_value = read_attr(source, span, b"test", diagnostics);

    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    diagnostics.append(&mut inner_diags);
    let inner_alts = flatten_segments(
        source,
        dialect,
        &inner_segments,
        diagnostics,
        property_paths,
    )?;

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
fn expand_choose(
    source: &str,
    dialect: Dialect,
    span: ByteSpan,
    diagnostics: &mut Vec<Diagnostic>,
    property_paths: &mut Vec<Spanned<String>>,
) -> Result<Vec<Alt>, u64> {
    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    diagnostics.append(&mut inner_diags);

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
            let test_value = read_attr(source, *child_span, b"test", diagnostics);
            let (when_segments, mut when_diags, _t) = capture_subtree(source, *child_span);
            diagnostics.append(&mut when_diags);
            let when_alts =
                flatten_segments(source, dialect, &when_segments, diagnostics, property_paths)?;
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
            diagnostics.append(&mut o_diags);
            local.extend(flatten_segments(
                source,
                dialect,
                &otherwise_segments,
                diagnostics,
                property_paths,
            )?);
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

/// Any dynamic tag MM-06a doesn't yet give real semantics to: recurse into
/// its body and use that body's own alternatives as-is, with no wrapper
/// text and no branch factor contributed by the container itself.
fn expand_transparent(
    source: &str,
    dialect: Dialect,
    span: ByteSpan,
    diagnostics: &mut Vec<Diagnostic>,
    property_paths: &mut Vec<Spanned<String>>,
) -> Result<Vec<Alt>, u64> {
    let (inner_segments, mut inner_diags, _truncated) = capture_subtree(source, span);
    diagnostics.append(&mut inner_diags);
    flatten_segments(
        source,
        dialect,
        &inner_segments,
        diagnostics,
        property_paths,
    )
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
/// directly.
fn union_walk(
    source: &str,
    dialect: Dialect,
    segments: &[BodySegment],
    diagnostics: &mut Vec<Diagnostic>,
    property_paths: &mut Vec<Spanned<String>>,
) -> Vec<Piece> {
    let mut pieces = Vec::new();

    for segment in segments {
        match segment {
            BodySegment::Text(text) => {
                let mut result =
                    placeholder::normalize_segment(&text.decoded, text.raw_span, dialect);
                property_paths.append(&mut result.property_paths);
                diagnostics.append(&mut result.diagnostics);
                pieces.push(Piece::Normalized {
                    text: result.text,
                    span_map: result.span_map,
                });
            }
            BodySegment::DynamicTag { name, span } if name == "include" => {
                pieces.push(Piece::Include {
                    raw: read_refid(source, *span),
                    span: *span,
                });
            }
            BodySegment::DynamicTag { name, span } if name == "choose" => {
                let (inner_segments, mut d, _t) = capture_subtree(source, *span);
                diagnostics.append(&mut d);
                for child in &inner_segments {
                    if let BodySegment::DynamicTag {
                        name: child_name,
                        span: child_span,
                    } = child
                    {
                        if child_name == "when" || child_name == "otherwise" {
                            let (child_segments, mut cd, _t) = capture_subtree(source, *child_span);
                            diagnostics.append(&mut cd);
                            pieces.extend(union_walk(
                                source,
                                dialect,
                                &child_segments,
                                diagnostics,
                                property_paths,
                            ));
                        }
                    }
                }
            }
            BodySegment::DynamicTag { span, .. } => {
                let (inner_segments, mut d, _t) = capture_subtree(source, *span);
                diagnostics.append(&mut d);
                pieces.extend(union_walk(
                    source,
                    dialect,
                    &inner_segments,
                    diagnostics,
                    property_paths,
                ));
            }
        }
    }

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
                    let shifted = base + off;
                    // A piece's own leading entry can land on the exact
                    // offset the previous piece's trailing entry already
                    // used (e.g. the previous piece ends right on a
                    // placeholder replacement, with no verbatim tail).
                    // The newer entry describes what actually starts at
                    // that position, so it wins.
                    if span_map.last().map(|(last_off, _)| *last_off) == Some(shifted) {
                        span_map.pop();
                    }
                    span_map.push((shifted, *raw));
                }
                text.push_str(t);
            }
            Piece::Include { raw, span } => {
                let offset = text.len() as u32;
                if span_map.last().map(|(last_off, _)| *last_off) == Some(offset) {
                    span_map.pop();
                }
                span_map.push((offset, span.start));
                text.push_str(&format!("/* atlas:include({raw}) */"));
            }
        }
    }

    SqlString { text, span_map }
}

fn read_attr(
    source: &str,
    span: ByteSpan,
    name: &[u8],
    diagnostics: &mut Vec<Diagnostic>,
) -> String {
    let attrs = scan_attributes(source.as_bytes(), span.start as usize, span.end as usize);
    let (value, mut diags) = attr_value_spanned(source, &attrs, name);
    diagnostics.append(&mut diags);
    value.map(|v| v.value).unwrap_or_default()
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

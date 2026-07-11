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
/// keyed per XML nesting level -- this module's own heap-worklist engines
/// (see the module-level "stack-diet" doc comment below) and (parse.rs)
/// `collect_mappings`. Pathologically deep nesting (thousands of levels)
/// would otherwise reach this deep before any branch-count check ever gets
/// a chance to bail out (each level's cartesian multiplication only happens
/// on the way back up, after the full depth-first descent) -- on a real
/// Rust call stack, a stack overflow aborts the process (uncatchable),
/// unlike every other anomaly in this crate; see the doc comment below for
/// why nesting depth no longer costs call-stack depth at all here.
pub(crate) const DEPTH_LIMIT: u32 = 256;

// ---------------------------------------------------------------------
// Stack-diet fix (measured 2026-07-11): dynamic-tag nesting depth used to
// cost one native Rust call-stack frame per level, through the mutual
// recursion `flatten_segments` -> `flatten_segments_inner` -> (per tag)
// `expand_if`/`expand_choose`/`expand_where`/.../`expand_transparent` ->
// `flatten_segments` (and separately `union_walk` -> `union_walk`, the
// `BranchLimitExceeded` fallback pass). Each level's frames were large
// enough (`Ctx` threaded by `&mut`, several `String`/`Vec` locals per
// `expand_*` call, `Alt`/`Piece` accumulators) that `DEPTH_LIMIT` (256)
// levels of nesting overflowed a 256 KiB thread stack in both debug and
// release, and even a 1 MiB thread in debug -- see
// `tests/small_stack_regression.rs`'s own doc comment for the measured
// numbers. On a 1 MiB-default-stack platform (Windows) this meant a
// merely-deep, not even pathological, `deep_if`/`deep_choose`-shaped mapper
// could abort the process from a debug build well under `DEPTH_LIMIT`
// itself ever firing -- violating this crate's "no panics on public paths"
// contract (`abort` isn't even a panic, so it's not catchable at all).
//
// Fix: every recursive descent that grows with dynamic-tag nesting depth
// (both the cartesian `flatten_segments` family and the
// `BranchLimitExceeded`-fallback `union_walk`) now runs on a `Vec`-backed
// heap worklist instead of the real call stack -- one [`Frame`]/[`UFrame`]
// per suspended level, driven by [`run`]/[`run_union`]. This is the same
// "frame owns everything its own call's now-unwound stack frame used to
// hold; `step` drives per-level progress; a finished frame's result flows
// back to whichever frame is now on top" shape the sibling `beans-xml`
// crate's `depth_engine`/`dispatch::BeansBodyFrame` modules document for
// the identical defect class there -- ported, not reinvented, adapted to
// this crate's own recursion shape (batis-xml has no single uniform
// recursive call site: nine different dynamic-tag kinds each need their
// own pre-recursion attribute reads and post-recursion wrapper transform,
// captured here as [`PendingKind`] rather than a handful of `Frame`
// variants).
//
// Two separate, self-contained engines -- [`Frame`]/[`BodyFrame`]/
// [`ChooseFrame`] (cartesian; `Completed = Result<Vec<Alt>, u64>`, the
// branch-limit-fallible walk) and [`UFrame`]/[`UnionFrame`]/
// [`UnionChooseFrame`] (union; `Completed = Vec<Piece>`, infallible) --
// rather than one shared `Frame` enum: `flatten_body` runs at most one of
// them per statement/fragment body (the union engine only runs as a
// from-scratch retry once the cartesian engine's own attempt has already
// been discarded, on a *fresh* `Ctx`), and their `Completed` types are
// incompatible, so folding them together would only add "can never
// actually happen" match arms to each side, for no benefit -- the same
// rationale `beans-xml::dispatch`'s own matching doc comment gives for
// keeping its `BeansBodyFrame` engine separate from `depth_engine::Frame`.
//
// `BranchLimitExceeded` short-circuit: the original recursive code
// propagated a `local.len() > BRANCH_LIMIT` (or cartesian-product
// overflow) `Err(n)` via `?` through every intervening `flatten_segments`
// call unchanged, with **no further work** at any of those levels (the
// `Ctx` accumulated so far is discarded wholesale by `flatten_body`'s own
// `Err` arm, which redoes the whole pass via `union_walk` on a fresh
// `Ctx`). [`run`] reproduces this exactly: once any frame finishes with
// `Err`, every still-suspended parent frame is popped without calling its
// own `deliver` (see [`run`]'s own comment) -- not "equivalent to", but the
// literal same "no observable work" the `?`-based unwind already did.
// ---------------------------------------------------------------------

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
    /// B38 (cold code review, minor): set while descending into an
    /// already-flagged unknown element's transparently-folded content, so
    /// its descendants don't each emit their own `UnknownElement` too --
    /// one typo'd wrapper (e.g. a misplaced `<resultMap>` nested inside a
    /// statement) shouldn't cascade into N extra diagnostics for every one
    /// of its own children (`<id>`, `<result>`, ...). Restored to its prior
    /// value after each such recursive call returns, so it never leaks
    /// into unrelated siblings.
    inside_unknown_element: bool,
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
        inside_unknown_element: false,
    };

    match run_flatten_engine(source, segments.to_vec(), &mut attempt) {
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
                inside_unknown_element: false,
            };
            let pieces = run_union_engine(source, segments.to_vec(), &mut ctx);
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

/// One dispatch unit for the main flattening/union walks: either a run of
/// one or more consecutive [`BodySegment::Text`] entries, or a single
/// [`BodySegment::DynamicTag`]. A run must be
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

/// Finds the exclusive end of the maximal run of consecutive
/// [`BodySegment::Text`] entries starting at `segments[start]` (`start`
/// itself must be a `Text` entry). Manual index-scanning rather than
/// [`group_runs`]: [`BodyFrame`]/[`UnionFrame`] own their `segments` as a
/// plain `Vec<BodySegment>` (see [`TextSegment`](crate::parse::TextSegment)'s
/// own doc comment for why), and [`group_runs`]'s own `RunItem<'a>` borrows
/// into its input slice -- a self-reference a struct can't hold across
/// suspension in a way that survives `Vec<Frame>` reallocation. Re-deriving
/// just the one grouping rule these two frame types actually need (adjacent
/// `Text` entries are one logical run) avoids that without duplicating
/// [`group_runs`] itself, which stays exactly as-is for
/// [`check_include_at_wrapper_boundary`]'s own (non-suspended, single-call)
/// use.
fn text_run_end(segments: &[BodySegment], start: usize) -> usize {
    let mut i = start;
    while matches!(segments.get(i), Some(BodySegment::Text(_))) {
        i += 1;
    }
    i
}

/// Borrows the [`crate::parse::TextSegment`] out of every entry in
/// `segments[start..end]` -- callers always pass a range [`text_run_end`]
/// itself just computed (all `BodySegment::Text`), so `filter_map` rather
/// than an infallible `map` is defense-in-depth, not a real possibility:
/// per CLAUDE.md rule 4 (no panics/unwrap/expect outside tests), a
/// mismatched entry is silently skipped rather than asserted against, even
/// though it can't currently happen given both call sites' own invariants.
fn text_run_refs(
    segments: &[BodySegment],
    start: usize,
    end: usize,
) -> Vec<&crate::parse::TextSegment> {
    segments[start..end]
        .iter()
        .filter_map(|s| match s {
            BodySegment::Text(t) => Some(t),
            BodySegment::DynamicTag { .. } => None,
        })
        .collect()
}

/// One in-progress cartesian expansion of a segment list -- what used to be
/// one activation record of `flatten_segments_inner` (see the module-level
/// stack-diet doc comment), now suspended on [`run`]'s own `Vec` instead of
/// the real call stack. `acc` is the same running cartesian-product
/// accumulator `flatten_segments_inner`'s own local `acc` was; `segments`
/// is owned (not borrowed), so this frame survives independently of
/// whatever pushed it.
struct BodyFrame {
    segments: Vec<BodySegment>,
    idx: usize,
    acc: Vec<Alt>,
    /// `Some` exactly while a child frame is on the stack above this one,
    /// resolving the dynamic tag at `segments[idx - 1]` -- set right before
    /// every [`Advance::Push`], consumed by [`Self::deliver`] once that
    /// child finishes. `None` whenever [`Self::step`] is the one running
    /// (mirrors `BeansBodyFrame`'s own "never called while a push it issued
    /// hasn't yet been resolved" invariant, from the sibling `beans-xml`
    /// crate).
    pending: Option<PendingBody>,
}

/// What [`BodyFrame`] is waiting to do with a suspended child's finished
/// `Vec<Alt>` once it comes back -- the exact per-tag post-recursion
/// transform each `expand_*` function used to run on its own recursive
/// call's return value, captured here instead so it can survive
/// suspension. `Identity` covers two original call shapes that both did
/// nothing to the delivered value: `<choose>`'s own finished result
/// (already fully formed by [`ChooseFrame`]) and a transparent/unknown
/// tag's body (no wrapper semantics of its own) -- the latter is the only
/// one that also needs [`PendingBody::restore_inside_unknown`].
enum PendingKind {
    Identity,
    If {
        test_value: String,
    },
    Conditional {
        condition: String,
        prepend: Option<String>,
    },
    Where,
    Set,
    Trim {
        prefix_with_sep: String,
        suffix_with_sep: String,
        prefix_overrides: Vec<String>,
        suffix_overrides: Vec<String>,
    },
    Foreach {
        open: String,
        close: String,
    },
    Dynamic {
        prepend: String,
    },
    Iterate {
        open: String,
        close: String,
    },
}

struct PendingBody {
    /// The dynamic tag's own span -- every non-`Identity` transform needs
    /// it as `with_prefix`/`with_suffix`'s synthetic-text `wrapper_start`,
    /// and [`PendingKind::Conditional`]'s `Piece::Prepend` needs it too.
    span: ByteSpan,
    kind: PendingKind,
    /// `Some(was_inside_unknown)` only for the transparent/unknown-tag
    /// case -- the value `ctx.inside_unknown_element` must be restored to
    /// once the child finishes, mirroring the original code's
    /// save-before/restore-after around its own (then-synchronous)
    /// recursive call. `None` for every other `PendingKind`, which never
    /// touches that flag.
    restore_inside_unknown: Option<bool>,
}

/// One in-progress `<choose>` expansion -- what used to be one activation
/// record of `expand_choose`'s own `for segment in &inner_segments` loop,
/// now suspended the same way [`BodyFrame`] is. A separate frame type (not
/// a `BodyFrame` variant): unlike every other dynamic tag, `<choose>` makes
/// *multiple*, sequential recursive descents (one per `<when>`/
/// `<otherwise>` child) before it has a finished result of its own, so it
/// needs its own child-index cursor and accumulator, distinct from
/// `BodyFrame`'s single-descent-per-tag shape.
struct ChooseFrame {
    segments: Vec<BodySegment>,
    idx: usize,
    local: Vec<Alt>,
    has_otherwise: bool,
    pending: Option<ChoosePending>,
}

enum ChoosePending {
    When { test_value: String },
    Otherwise,
}

/// One suspended cartesian-engine call -- either the general per-tag
/// dispatch loop ([`BodyFrame`]) or a `<choose>`'s own multi-descent loop
/// ([`ChooseFrame`]). See the module-level stack-diet doc comment for why
/// this stays a separate engine from [`UFrame`] rather than folding the two
/// together.
enum Frame {
    Body(BodyFrame),
    Choose(ChooseFrame),
}

impl Frame {
    fn step(&mut self, source: &str, ctx: &mut Ctx) -> Advance {
        match self {
            Frame::Body(b) => b.step(source, ctx),
            Frame::Choose(c) => c.step(source, ctx),
        }
    }

    fn deliver(&mut self, source: &str, ctx: &mut Ctx, alts: Vec<Alt>) -> Advance {
        match self {
            Frame::Body(b) => b.deliver(source, ctx, alts),
            Frame::Choose(c) => c.deliver(alts),
        }
    }
}

/// [`Frame::step`]/[`Frame::deliver`]'s own result -- "advance in place /
/// descend / return", the same three-way split the sibling `beans-xml`
/// crate's `depth_engine::Advance`/`dispatch::BeansAdvance` document (see
/// this module's own stack-diet comment for the shared lineage).
/// `Finished` carries the frame's own `Result<Vec<Alt>, u64>` directly
/// (rather than a separate `Completed` type, unlike the `beans-xml`
/// sibling's engines) since both `BodyFrame` and `ChooseFrame` produce
/// exactly that same type -- no wrapping needed before delivering it to
/// whatever is now on top.
enum Advance {
    /// Descend: push `frame` and re-enter [`run`]'s own loop with it on
    /// top -- the frame underneath (which requested this) is left exactly
    /// as it was; it only resumes once the pushed frame eventually
    /// finishes (see `Frame::deliver`).
    Push(Box<Frame>),
    /// Made progress without changing the stack's shape.
    Continue,
    /// Return: this frame has nothing left to do.
    Finished(Result<Vec<Alt>, u64>),
}

/// What pushing a nested cartesian descent decided to do -- the check-then-
/// increment half of what `flatten_segments`'s own wrapper used to do
/// inline before every recursive call. `Immediate` is the depth-cap-reached
/// case: the original code's `Ok(vec![empty_alt()])` early return never
/// actually recursed at all, so this engine doesn't push a frame for it
/// either -- see [`descend_body`].
enum Descend {
    Push(Box<Frame>),
    Immediate(Vec<Alt>),
}

/// Depth-checked equivalent of calling `flatten_segments` recursively (see
/// the module-level stack-diet doc comment) -- the single choke point every
/// `BodyFrame`/`ChooseFrame` descent into a nested segment list goes
/// through, so [`DEPTH_LIMIT`] is enforced uniformly regardless of which
/// dynamic tag triggered it. Mirrors `flatten_segments`'s own "check, then
/// increment, only if under the cap" order exactly; the matching decrement
/// happens once in [`run`], when the pushed `Frame::Body` this call
/// produces is eventually popped (whether `Finished(Ok(_))` or
/// `Finished(Err(_))` -- see `run`'s own comment for why the latter doesn't
/// need special-casing here).
fn descend_body(inner_segments: Vec<BodySegment>, ctx: &mut Ctx) -> Descend {
    if ctx.depth >= DEPTH_LIMIT {
        ctx.diagnostics.push(nesting_limit_diagnostic());
        return Descend::Immediate(vec![empty_alt()]);
    }
    ctx.depth += 1;
    Descend::Push(Box::new(Frame::Body(BodyFrame::new(inner_segments))))
}

impl BodyFrame {
    fn new(segments: Vec<BodySegment>) -> Self {
        BodyFrame {
            segments,
            idx: 0,
            acc: vec![empty_alt()],
            pending: None,
        }
    }

    /// Begins a nested descent for the dynamic tag just consumed at
    /// `self.idx - 1`: stashes `kind`/`restore_inside_unknown` as this
    /// frame's own [`PendingBody`], then either pushes a child [`Frame`]
    /// (the common case) or -- if [`DEPTH_LIMIT`] is already reached --
    /// resolves the descent synchronously via [`Self::deliver`], exactly as
    /// `descend_body`'s own `Descend::Immediate` documents.
    fn begin_descend(
        &mut self,
        source: &str,
        ctx: &mut Ctx,
        span: ByteSpan,
        kind: PendingKind,
        restore_inside_unknown: Option<bool>,
        inner_segments: Vec<BodySegment>,
    ) -> Advance {
        self.pending = Some(PendingBody {
            span,
            kind,
            restore_inside_unknown,
        });
        match descend_body(inner_segments, ctx) {
            Descend::Push(frame) => Advance::Push(frame),
            Descend::Immediate(alts) => self.deliver(source, ctx, alts),
        }
    }

    fn step(&mut self, source: &str, ctx: &mut Ctx) -> Advance {
        loop {
            let Some(seg) = self.segments.get(self.idx) else {
                return Advance::Finished(Ok(std::mem::take(&mut self.acc)));
            };
            match seg {
                BodySegment::Text(_) => {
                    let start = self.idx;
                    let end = text_run_end(&self.segments, start);
                    self.idx = end;
                    let texts = text_run_refs(&self.segments, start, end);
                    let piece = normalize_run(&texts, ctx);
                    for alt in &mut self.acc {
                        alt.pieces.push(piece.clone());
                    }
                }
                BodySegment::DynamicTag { name, span } => {
                    let name = name.clone();
                    let span = *span;
                    self.idx += 1;
                    match name.as_str() {
                        "include" => {
                            record_include(source, span, ctx);
                            let raw = read_refid(source, span);
                            for alt in &mut self.acc {
                                alt.pieces.push(Piece::Include {
                                    raw: raw.clone(),
                                    span,
                                });
                            }
                        }
                        "bind" => {
                            // Contributes no text; its expression lives in
                            // an attribute (`value`), never in a
                            // Text/CDATA event, so MM-07's normalization
                            // never sees it -- record it here.
                            let value = read_attr(source, span, b"value", ctx);
                            if !value.is_empty() {
                                ctx.property_paths.push(Spanned { value, span });
                            }
                        }
                        "choose" => {
                            let (inner_segments, mut inner_diags, _t) =
                                capture_subtree(source, span);
                            ctx.diagnostics.append(&mut inner_diags);
                            // Unlike every other dynamic tag, `<choose>`
                            // doesn't itself correspond to a
                            // `flatten_segments` call (only its own
                            // `<when>`/`<otherwise>` children's bodies do,
                            // each via `descend_body` inside
                            // `ChooseFrame::step`) -- so this push is
                            // unconditional, no `descend_body`/`DEPTH_LIMIT`
                            // check here, exactly matching the original
                            // `expand_choose` being called directly (not
                            // through the `flatten_segments` wrapper) from
                            // `flatten_segments_inner`.
                            self.pending = Some(PendingBody {
                                span,
                                kind: PendingKind::Identity,
                                restore_inside_unknown: None,
                            });
                            return Advance::Push(Box::new(Frame::Choose(ChooseFrame::new(
                                inner_segments,
                            ))));
                        }
                        "if" => {
                            let test_value = read_attr(source, span, b"test", ctx);
                            let (inner_segments, mut inner_diags, _t) =
                                capture_subtree(source, span);
                            ctx.diagnostics.append(&mut inner_diags);
                            return self.begin_descend(
                                source,
                                ctx,
                                span,
                                PendingKind::If { test_value },
                                None,
                                inner_segments,
                            );
                        }
                        "where" => {
                            let (inner_segments, mut inner_diags, _t) =
                                capture_subtree(source, span);
                            ctx.diagnostics.append(&mut inner_diags);
                            check_include_at_wrapper_boundary("where", &inner_segments, ctx);
                            return self.begin_descend(
                                source,
                                ctx,
                                span,
                                PendingKind::Where,
                                None,
                                inner_segments,
                            );
                        }
                        "set" => {
                            let (inner_segments, mut inner_diags, _t) =
                                capture_subtree(source, span);
                            ctx.diagnostics.append(&mut inner_diags);
                            check_include_at_wrapper_boundary("set", &inner_segments, ctx);
                            return self.begin_descend(
                                source,
                                ctx,
                                span,
                                PendingKind::Set,
                                None,
                                inner_segments,
                            );
                        }
                        "trim" => {
                            let prefix = read_attr(source, span, b"prefix", ctx);
                            let suffix = read_attr(source, span, b"suffix", ctx);
                            let prefix_overrides =
                                split_overrides(&read_attr(source, span, b"prefixOverrides", ctx));
                            let suffix_overrides =
                                split_overrides(&read_attr(source, span, b"suffixOverrides", ctx));
                            let prefix_with_sep = if prefix.is_empty() {
                                String::new()
                            } else {
                                format!("{prefix} ")
                            };
                            let suffix_with_sep = if suffix.is_empty() {
                                String::new()
                            } else {
                                format!(" {suffix}")
                            };
                            let (inner_segments, mut inner_diags, _t) =
                                capture_subtree(source, span);
                            ctx.diagnostics.append(&mut inner_diags);
                            check_include_at_wrapper_boundary("trim", &inner_segments, ctx);
                            return self.begin_descend(
                                source,
                                ctx,
                                span,
                                PendingKind::Trim {
                                    prefix_with_sep,
                                    suffix_with_sep,
                                    prefix_overrides,
                                    suffix_overrides,
                                },
                                None,
                                inner_segments,
                            );
                        }
                        "foreach" => {
                            let open = read_attr(source, span, b"open", ctx);
                            let close = read_attr(source, span, b"close", ctx);
                            let (inner_segments, mut inner_diags, _t) =
                                capture_subtree(source, span);
                            ctx.diagnostics.append(&mut inner_diags);
                            return self.begin_descend(
                                source,
                                ctx,
                                span,
                                PendingKind::Foreach { open, close },
                                None,
                                inner_segments,
                            );
                        }
                        "dynamic" => {
                            let prepend = read_attr(source, span, b"prepend", ctx);
                            let (inner_segments, mut inner_diags, _t) =
                                capture_subtree(source, span);
                            ctx.diagnostics.append(&mut inner_diags);
                            return self.begin_descend(
                                source,
                                ctx,
                                span,
                                PendingKind::Dynamic { prepend },
                                None,
                                inner_segments,
                            );
                        }
                        "iterate" => {
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
                            let (inner_segments, mut inner_diags, _t) =
                                capture_subtree(source, span);
                            ctx.diagnostics.append(&mut inner_diags);
                            return self.begin_descend(
                                source,
                                ctx,
                                span,
                                PendingKind::Iterate { open, close },
                                None,
                                inner_segments,
                            );
                        }
                        _ if name.starts_with("is") => {
                            let condition = synthesize_condition(&name, source, span, ctx);
                            let prepend = read_attr_opt(source, span, b"prepend", ctx);
                            let (inner_segments, mut inner_diags, _t) =
                                capture_subtree(source, span);
                            ctx.diagnostics.append(&mut inner_diags);
                            return self.begin_descend(
                                source,
                                ctx,
                                span,
                                PendingKind::Conditional { condition, prepend },
                                None,
                                inner_segments,
                            );
                        }
                        _ => {
                            // A14 (cold code review, major): transparent
                            // passthrough for anything with no special
                            // semantics -- but flag it first unless it's a
                            // known-ignorable element (see
                            // is_known_ignorable_element's doc comment).
                            // Before this, a genuine typo like <iff> for
                            // <if> folded its content in unconditionally
                            // with no diagnostic at all, identical to a
                            // deliberately out-of-scope tag -- the recovery
                            // (still folding the content in transparently,
                            // so a typo'd wrapper doesn't also lose its
                            // body text) is deliberate and kept; only the
                            // silence is fixed.
                            //
                            // B38 (cold code review, minor): a genuinely
                            // unknown element's content is still walked by
                            // this same recursive descent (that's the
                            // "folded in transparently" part), so its own
                            // children hit this same catch-all too -- e.g.
                            // a misplaced <resultMap> nested inside a
                            // statement flagged itself AND its
                            // <id>/<result> children, one authoring mistake
                            // producing N+1 diagnostics. Only the outermost
                            // unknown element in a chain is flagged;
                            // ctx.inside_unknown_element suppresses the
                            // rest and is restored after this element's own
                            // subtree is done, so it never leaks into an
                            // unrelated sibling elsewhere in the tree.
                            let is_unknown = !crate::parse::is_known_ignorable_element(&name);
                            if is_unknown && !ctx.inside_unknown_element {
                                ctx.diagnostics.push(Diagnostic {
                                    code: DiagCode::UnknownElement,
                                    span: Some(span),
                                    message: format!(
                                        "unrecognized element <{name}> in dynamic position -- not a \
                                         known MyBatis/iBatis dynamic tag, and not one of this \
                                         crate's known-ignorable elements; its content is still \
                                         folded in transparently (possible typo?)"
                                    ),
                                });
                            }
                            let was_inside_unknown = ctx.inside_unknown_element;
                            if is_unknown {
                                ctx.inside_unknown_element = true;
                            }
                            let (inner_segments, mut inner_diags, _t) =
                                capture_subtree(source, span);
                            ctx.diagnostics.append(&mut inner_diags);
                            return self.begin_descend(
                                source,
                                ctx,
                                span,
                                PendingKind::Identity,
                                Some(was_inside_unknown),
                                inner_segments,
                            );
                        }
                    }
                }
            }
        }
    }

    fn deliver(&mut self, source: &str, ctx: &mut Ctx, alts: Vec<Alt>) -> Advance {
        // No panics/unwrap/expect outside tests (CLAUDE.md rule 4): `run`
        // only ever calls `deliver` on the frame that itself issued the
        // most recent `Advance::Push` (see `run`'s own doc comment), so
        // `self.pending` is always `Some` here in practice -- but rather
        // than assert that invariant, treat an unexpected `None` as "no
        // pending work to fold in", the same safe no-op every other
        // "can't currently happen" spot in this engine falls back to (see
        // `text_run_refs`'s own doc comment).
        let Some(pending) = self.pending.take() else {
            return Advance::Continue;
        };
        if let Some(prev) = pending.restore_inside_unknown {
            ctx.inside_unknown_element = prev;
        }
        match finish_pending(source, pending.span, pending.kind, alts) {
            Err(n) => Advance::Finished(Err(n)),
            Ok(local) => match try_combine(&self.acc, &local) {
                Err(n) => Advance::Finished(Err(n)),
                Ok(combined) => {
                    self.acc = combined;
                    Advance::Continue
                }
            },
        }
    }
}

impl ChooseFrame {
    fn new(segments: Vec<BodySegment>) -> Self {
        ChooseFrame {
            segments,
            idx: 0,
            local: Vec::new(),
            has_otherwise: false,
            pending: None,
        }
    }

    fn step(&mut self, source: &str, ctx: &mut Ctx) -> Advance {
        loop {
            let Some(seg) = self.segments.get(self.idx) else {
                if !self.has_otherwise {
                    self.local.push(empty_alt());
                }
                return Advance::Finished(Ok(std::mem::take(&mut self.local)));
            };
            let BodySegment::DynamicTag { name, span } = seg else {
                self.idx += 1;
                continue; // stray text between <when>/<otherwise> -- no branch content
            };
            let name = name.clone();
            let span = *span;
            self.idx += 1;

            if name == "when" {
                let test_value = read_attr(source, span, b"test", ctx);
                let (when_segments, mut when_diags, _t) = capture_subtree(source, span);
                ctx.diagnostics.append(&mut when_diags);
                self.pending = Some(ChoosePending::When { test_value });
                return match descend_body(when_segments, ctx) {
                    Descend::Push(frame) => Advance::Push(frame),
                    Descend::Immediate(alts) => self.deliver(alts),
                };
            } else if name == "otherwise" {
                self.has_otherwise = true;
                let (otherwise_segments, mut o_diags, _t) = capture_subtree(source, span);
                ctx.diagnostics.append(&mut o_diags);
                self.pending = Some(ChoosePending::Otherwise);
                return match descend_body(otherwise_segments, ctx) {
                    Descend::Push(frame) => Advance::Push(frame),
                    Descend::Immediate(alts) => self.deliver(alts),
                };
            } else {
                // Cold code review B7: anything else (including a stray
                // <include>) used to vanish silently here -- no text
                // contribution, no diagnostic, nothing telling a consumer
                // it was even there.
                ctx.diagnostics.push(Diagnostic {
                    code: DiagCode::UnknownElement,
                    span: Some(span),
                    message: format!(
                        "<choose> child <{name}> is neither <when> nor <otherwise> -- ignored"
                    ),
                });
                // `self.local` didn't change, so the original per-iteration
                // BRANCH_LIMIT check this mirrors is a no-op here -- loop
                // straight to the next child.
            }
        }
    }

    fn deliver(&mut self, alts: Vec<Alt>) -> Advance {
        // See BodyFrame::deliver's own comment: `self.pending` is always
        // `Some` here in practice, but an unexpected `None` falls back to
        // a safe no-op rather than a panic.
        let Some(pending) = self.pending.take() else {
            return Advance::Continue;
        };
        match pending {
            ChoosePending::When { test_value } => {
                for alt in alts {
                    let mut conditions = vec![test_value.clone()];
                    conditions.extend(alt.conditions);
                    self.local.push(Alt {
                        pieces: alt.pieces,
                        conditions,
                    });
                }
            }
            ChoosePending::Otherwise => {
                self.local.extend(alts);
            }
        }
        if self.local.len() as u64 > BRANCH_LIMIT as u64 {
            return Advance::Finished(Err(self.local.len() as u64));
        }
        Advance::Continue
    }
}

/// Applies a suspended [`BodyFrame`]'s [`PendingKind`] to its now-delivered
/// child result -- the exact per-tag post-recursion transform each original
/// `expand_*` function ran on its own recursive call's return value (`<if>`/
/// `<isNotEmpty>`-etc.'s condition-prefixing plus [`BRANCH_LIMIT`] check;
/// `<where>`/`<set>`/`<trim>`/`<foreach>`/`<dynamic>`/`<iterate>`'s
/// per-alternative wrapper-text transform; `Identity` for `<choose>` and
/// the transparent/unknown catch-all, neither of which had any transform of
/// its own). No `ctx` needed: none of these transforms ever touched it (the
/// pre-recursion attribute reads that did are already done, in
/// `BodyFrame::step`, before this ever runs).
fn finish_pending(
    source: &str,
    span: ByteSpan,
    kind: PendingKind,
    inner_alts: Vec<Alt>,
) -> Result<Vec<Alt>, u64> {
    match kind {
        PendingKind::Identity => Ok(inner_alts),
        PendingKind::If { test_value } => {
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
        PendingKind::Conditional { condition, prepend } => {
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
        PendingKind::Where => Ok(inner_alts
            .into_iter()
            .map(|alt| {
                let inner_sql = assemble(&alt.pieces);
                let pieces = if inner_sql.text.trim().is_empty() {
                    Vec::new()
                } else {
                    let strip_n = leading_and_or_strip_len(&inner_sql.text);
                    vec![to_piece(with_prefix(
                        source, inner_sql, span.start, strip_n, "WHERE ",
                    ))]
                };
                Alt {
                    pieces,
                    conditions: alt.conditions,
                }
            })
            .collect()),
        PendingKind::Set => Ok(inner_alts
            .into_iter()
            .map(|alt| {
                let inner_sql = assemble(&alt.pieces);
                let pieces = if inner_sql.text.trim().is_empty() {
                    Vec::new()
                } else {
                    let leading_strip = leading_comma_strip_len(&inner_sql.text);
                    let trailing_strip = trailing_comma_strip_len(&inner_sql.text)
                        .min(inner_sql.text.len() - leading_strip);
                    let with_lead =
                        with_prefix(source, inner_sql, span.start, leading_strip, "SET ");
                    vec![to_piece(with_suffix_strip(with_lead, trailing_strip))]
                };
                Alt {
                    pieces,
                    conditions: alt.conditions,
                }
            })
            .collect()),
        PendingKind::Trim {
            prefix_with_sep,
            suffix_with_sep,
            prefix_overrides,
            suffix_overrides,
        } => Ok(inner_alts
            .into_iter()
            .map(|alt| {
                let inner_sql = assemble(&alt.pieces);
                let pieces = if inner_sql.text.trim().is_empty() {
                    Vec::new()
                } else {
                    let lead_strip = leading_override_strip_len(&inner_sql.text, &prefix_overrides);
                    let trail_strip =
                        trailing_override_strip_len(&inner_sql.text, &suffix_overrides)
                            .min(inner_sql.text.len() - lead_strip);
                    let with_lead =
                        with_prefix(source, inner_sql, span.start, lead_strip, &prefix_with_sep);
                    let trimmed = with_suffix_strip(with_lead, trail_strip);
                    vec![to_piece(with_suffix(trimmed, span.start, &suffix_with_sep))]
                };
                Alt {
                    pieces,
                    conditions: alt.conditions,
                }
            })
            .collect()),
        PendingKind::Foreach { open, close } => Ok(inner_alts
            .into_iter()
            .map(|alt| {
                let inner_sql = assemble(&alt.pieces);
                let wrapped = with_suffix(
                    with_prefix(source, inner_sql, span.start, 0, &open),
                    span.start,
                    &close,
                );
                Alt {
                    pieces: vec![to_piece(wrapped)],
                    conditions: alt.conditions,
                }
            })
            .collect()),
        PendingKind::Dynamic { prepend } => Ok(inner_alts
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
                        source,
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
            .collect()),
        PendingKind::Iterate { open, close } => Ok(inner_alts
            .into_iter()
            .map(|alt| {
                let inner_sql = assemble(&alt.pieces);
                let wrapped = with_suffix(
                    with_prefix(source, inner_sql, span.start, 0, &open),
                    span.start,
                    &close,
                );
                Alt {
                    pieces: vec![to_piece(wrapped)],
                    conditions: alt.conditions,
                }
            })
            .collect()),
    }
}

/// Drives `stack` to completion -- see the module-level stack-diet doc
/// comment for the full step/descend/return framing. `stack` must start
/// with exactly one frame; every further frame is pushed/popped internally
/// as nested dynamic-tag resolution demands.
fn run(mut stack: Vec<Frame>, source: &str, ctx: &mut Ctx) -> Result<Vec<Alt>, u64> {
    let mut incoming: Option<Vec<Alt>> = None;
    while let Some(top) = stack.last_mut() {
        let advance = match incoming.take() {
            Some(alts) => top.deliver(source, ctx, alts),
            None => top.step(source, ctx),
        };
        match advance {
            Advance::Push(frame) => stack.push(*frame),
            Advance::Continue => {}
            Advance::Finished(result) => {
                // `stack.pop()` is always `Some` here (the frame `top` just
                // advanced is still on top; nothing above has pushed or
                // popped since) -- no panics/unwrap/expect outside tests
                // (CLAUDE.md rule 4), so this is `if let` rather than an
                // assert. If it were ever unexpectedly `None`, `stack` was
                // already empty, which the `Ok`/`Err` handling below
                // already treats correctly either way (`stack.is_empty()`
                // is then trivially true, and the drain loop below has
                // nothing to drain).
                if let Some(frame) = stack.pop() {
                    if matches!(frame, Frame::Body(_)) {
                        ctx.depth -= 1;
                    }
                }
                match result {
                    Ok(alts) => {
                        if stack.is_empty() {
                            return Ok(alts);
                        }
                        incoming = Some(alts);
                    }
                    Err(n) => {
                        // BranchLimitExceeded short-circuit -- see the
                        // module-level stack-diet doc comment's own
                        // paragraph on this: the original `?`-based unwind
                        // did no further work at any intervening
                        // `flatten_segments` call once `Err` first
                        // appeared, so draining every remaining frame here
                        // without calling its own `deliver` reproduces
                        // that exactly, not just "equivalently". Depth is
                        // still unwound correctly for every drained
                        // `Frame::Body` (even though `flatten_body`'s own
                        // `Err` arm discards this whole `Ctx` right after,
                        // making it moot in practice -- see
                        // `descend_body`'s own doc comment) purely so
                        // `ctx` never carries a stale, unbalanced `depth`
                        // past this function's return under any future
                        // caller.
                        for f in stack.drain(..) {
                            if matches!(f, Frame::Body(_)) {
                                ctx.depth -= 1;
                            }
                        }
                        return Err(n);
                    }
                }
            }
        }
    }
    // Structurally unreachable in practice: every call site
    // (`run_flatten_engine`, and every `Advance::Push`/`Descend::Push`
    // this engine issues to itself) starts/keeps `stack` non-empty, and
    // the loop above only exits via an explicit `return` once popping
    // empties it. A safe, well-typed fallback rather than a panic if that
    // invariant were ever violated (CLAUDE.md rule 4).
    Ok(Vec::new())
}

/// Entry point for the cartesian engine -- what a top-level
/// `flatten_segments(source, segments, ctx)` call used to be. Goes through
/// [`descend_body`] just like every nested descent does (see that
/// function's own doc comment), so a `DEPTH_LIMIT` of `0` would behave
/// identically here and at every nested call site -- moot in practice
/// (`ctx.depth` always starts at `0` and `DEPTH_LIMIT` is `256`), kept for
/// that uniformity rather than special-casing the entry point.
fn run_flatten_engine(
    source: &str,
    segments: Vec<BodySegment>,
    ctx: &mut Ctx,
) -> Result<Vec<Alt>, u64> {
    match descend_body(segments, ctx) {
        Descend::Push(frame) => run(vec![*frame], source, ctx),
        Descend::Immediate(alts) => Ok(alts),
    }
}

/// MyBatis expands `<include>` *before*
/// dynamic evaluation, so a `<where>`/`<set>`/`<trim>`'s own leading-AND/OR
/// or trailing-comma rule sees the fragment's actual substituted text.
/// This crate flattens with the include token still in place (fragment
/// substitution is the consumer's job downstream), so if the substituted
/// fragment itself starts with `AND `/`OR ` or ends with a trailing comma
/// right where the wrapper's own strip rule would have applied, the
/// consumer can end up with `WHERE AND x = 1` or a kept trailing comma —
/// see the README's include section and `IncludeTarget`'s rustdoc for the
/// full contract. Rather than silently leaving this to be discovered, flag
/// the exact spot it bites: an `<include>` that is the first or last
/// non-whitespace direct child of a where/set/trim wrapper (a wrapper
/// whose *only* content is an include token is exactly the "must be
/// treated as conditional" case, since the fragment might expand to
/// nothing).
fn check_include_at_wrapper_boundary(
    wrapper_name: &str,
    inner_segments: &[BodySegment],
    ctx: &mut Ctx,
) {
    let is_whitespace_only_text =
        |texts: &[&crate::parse::TextSegment]| texts.iter().all(|t| t.decoded.trim().is_empty());
    let runs = group_runs(inner_segments);
    let content: Vec<&RunItem> = runs
        .iter()
        .filter(|item| !matches!(item, RunItem::Text(texts) if is_whitespace_only_text(texts)))
        .collect();

    let emit = |span: ByteSpan, ctx: &mut Ctx| {
        ctx.diagnostics.push(Diagnostic {
            code: DiagCode::IncludeAtWrapperBoundary,
            span: Some(span),
            message: format!(
                "<include> is the first or last non-whitespace content inside <{wrapper_name}> -- since <include> expands before dynamic evaluation in MyBatis/iBatis, re-apply {wrapper_name}'s leading AND/OR or trailing-comma rule to the substituted fragment text (a wrapper whose only content is this include must be treated as conditional)"
            ),
        });
    };

    // A12 (cold code review, major): the early `return` after the first-
    // position check must only fire when first and last are the *same*
    // element (a single include as the wrapper's only content) -- it used
    // to fire unconditionally whenever the first element was an include,
    // which silently skipped the last-position check whenever the wrapper
    // had 2+ content elements starting with an include (e.g. an include
    // first *and* a different include last). `content.len() == 1` is
    // exactly the "first and last are the same element" condition.
    if let Some(RunItem::Tag { name, span }) = content.first() {
        if name.as_str() == "include" {
            emit(**span, ctx);
            if content.len() == 1 {
                return; // first and last are the same element -- don't double-report it
            }
        }
    }
    if let Some(RunItem::Tag { name, span }) = content.last() {
        if name.as_str() == "include" {
            emit(**span, ctx);
        }
    }
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

/// Removes the first [`Piece::Prepend`] found (if any) — see
/// [`PendingKind::Dynamic`]'s own use in [`finish_pending`].
fn remove_first_prepend(pieces: &mut Vec<Piece>) {
    if let Some(pos) = pieces
        .iter()
        .position(|p| matches!(p, Piece::Prepend { .. }))
    {
        pieces.remove(pos);
    }
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

/// One in-progress [`UnionFrame`] walk -- what used to be one activation
/// record of `union_walk` (the `BranchLimitExceeded` over-cap fallback:
/// concatenates every branch's content once, no cartesian multiplication,
/// in document order — "each branch's content concatenated once" rather
/// than a syntactically valid query). Structurally a single linear walk,
/// so each segment is visited exactly once regardless of tree shape --
/// wrapper tags are treated the same as transparent containers here (their
/// prefix/suffix semantics only make sense per-branch, and there are no
/// branches in a union). Owned `segments`, no lifetime: see
/// [`text_run_end`]'s own doc comment for why.
struct UnionFrame {
    segments: Vec<BodySegment>,
    idx: usize,
    pieces: Vec<Piece>,
}

/// One in-progress `<choose>`-in-a-union walk -- what used to be
/// `union_walk`'s own `"choose"` arm's `for child in &inner_segments` loop.
/// A separate frame type from [`UnionFrame`] for the same reason
/// [`ChooseFrame`] is separate from [`BodyFrame`] in the cartesian engine:
/// it walks a *different* segment list (the choose's own `<when>`/
/// `<otherwise>` children, picking only those two names) and makes
/// multiple sequential recursive descents before it's done, one per
/// matching child.
struct UnionChooseFrame {
    children: Vec<BodySegment>,
    idx: usize,
    pieces: Vec<Piece>,
}

/// One suspended union-engine call -- either the general linear walk
/// ([`UnionFrame`]) or a `<choose>`'s own children loop
/// ([`UnionChooseFrame`]). See the module-level stack-diet doc comment for
/// why this is a separate engine from [`Frame`] rather than folding the two
/// together.
enum UFrame {
    Union(UnionFrame),
    Choose(UnionChooseFrame),
}

impl UFrame {
    fn step(&mut self, source: &str, ctx: &mut Ctx) -> UnionAdvance {
        match self {
            UFrame::Union(u) => u.step(source, ctx),
            UFrame::Choose(c) => c.step(source, ctx),
        }
    }

    fn deliver(&mut self, pieces: Vec<Piece>) -> UnionAdvance {
        match self {
            UFrame::Union(u) => u.deliver(pieces),
            UFrame::Choose(c) => c.deliver(pieces),
        }
    }
}

/// [`UFrame::step`]/[`UFrame::deliver`]'s own result -- same three-way
/// "advance in place / descend / return" split as [`Advance`], specialized
/// to `Vec<Piece>` (the union engine can never fail, unlike the cartesian
/// one -- see the module-level stack-diet doc comment).
enum UnionAdvance {
    Push(Box<UFrame>),
    Continue,
    Finished(Vec<Piece>),
}

/// What pushing a nested union descent decided to do -- see [`Descend`]'s
/// own doc comment, mirrored here for the union engine. `Immediate` matches
/// `union_walk`'s own depth-cap-reached early return (`Vec::new()`, no
/// recursion at all).
enum UnionDescend {
    Push(Box<UFrame>),
    Immediate(Vec<Piece>),
}

/// Depth-checked equivalent of calling `union_walk` recursively -- the
/// single choke point every [`UnionFrame`]/[`UnionChooseFrame`] descent
/// into a nested segment list goes through. Mirrors `union_walk`'s own
/// "check, then increment, only if under the cap" order exactly; the
/// matching decrement happens once in [`run_union`], when the pushed
/// `UFrame::Union` this call produces is eventually popped.
fn descend_union(inner_segments: Vec<BodySegment>, ctx: &mut Ctx) -> UnionDescend {
    if ctx.depth >= DEPTH_LIMIT {
        ctx.diagnostics.push(nesting_limit_diagnostic());
        return UnionDescend::Immediate(Vec::new());
    }
    ctx.depth += 1;
    UnionDescend::Push(Box::new(UFrame::Union(UnionFrame::new(inner_segments))))
}

impl UnionFrame {
    fn new(segments: Vec<BodySegment>) -> Self {
        UnionFrame {
            segments,
            idx: 0,
            pieces: Vec::new(),
        }
    }

    fn step(&mut self, source: &str, ctx: &mut Ctx) -> UnionAdvance {
        loop {
            let Some(seg) = self.segments.get(self.idx) else {
                return UnionAdvance::Finished(std::mem::take(&mut self.pieces));
            };
            match seg {
                BodySegment::Text(_) => {
                    let start = self.idx;
                    let end = text_run_end(&self.segments, start);
                    self.idx = end;
                    let texts = text_run_refs(&self.segments, start, end);
                    self.pieces.push(normalize_run(&texts, ctx));
                }
                BodySegment::DynamicTag { name, span } if name == "include" => {
                    let span = *span;
                    self.idx += 1;
                    record_include(source, span, ctx);
                    self.pieces.push(Piece::Include {
                        raw: read_refid(source, span),
                        span,
                    });
                }
                BodySegment::DynamicTag { name, span } if name == "bind" => {
                    let span = *span;
                    self.idx += 1;
                    let value = read_attr(source, span, b"value", ctx);
                    if !value.is_empty() {
                        ctx.property_paths.push(Spanned { value, span });
                    }
                }
                BodySegment::DynamicTag { name, span } if name == "iterate" => {
                    let span = *span;
                    self.idx += 1;
                    if let Some(property) = read_attr_opt(source, span, b"property", ctx) {
                        if !property.is_empty() {
                            ctx.property_paths.push(Spanned {
                                value: property,
                                span,
                            });
                        }
                    }
                    let (inner_segments, mut d, _t) = capture_subtree(source, span);
                    ctx.diagnostics.append(&mut d);
                    return match descend_union(inner_segments, ctx) {
                        UnionDescend::Push(frame) => UnionAdvance::Push(frame),
                        UnionDescend::Immediate(pieces) => self.deliver(pieces),
                    };
                }
                BodySegment::DynamicTag { name, span } if name == "choose" => {
                    let span = *span;
                    self.idx += 1;
                    let (inner_segments, mut d, _t) = capture_subtree(source, span);
                    ctx.diagnostics.append(&mut d);
                    // Like `ChooseFrame` in the cartesian engine, this push
                    // is unconditional -- `<choose>`'s own children loop
                    // isn't itself depth-checked (only the `union_walk`
                    // calls it makes for each matching child are, via
                    // `descend_union` inside `UnionChooseFrame::step`),
                    // matching the original inline `for child in
                    // &inner_segments` loop having no `DEPTH_LIMIT` check
                    // of its own.
                    return UnionAdvance::Push(Box::new(UFrame::Choose(UnionChooseFrame::new(
                        inner_segments,
                    ))));
                }
                BodySegment::DynamicTag { span, .. } => {
                    // Note: unlike BodyFrame's catch-all, this one is NOT
                    // "unrecognized element" -- the union walk deliberately
                    // folds every wrapper tag (<if>, <where>, <trim>, ...)
                    // transparently too, since the union representation
                    // never tries to encode branch structure at all
                    // (that's the whole point of the BranchLimitExceeded
                    // fallback). A14's unknown-element diagnosis only
                    // applies to the cartesian path's own catch-all, where
                    // it's a genuine "nothing matched" case.
                    let span = *span;
                    self.idx += 1;
                    let (inner_segments, mut d, _t) = capture_subtree(source, span);
                    ctx.diagnostics.append(&mut d);
                    return match descend_union(inner_segments, ctx) {
                        UnionDescend::Push(frame) => UnionAdvance::Push(frame),
                        UnionDescend::Immediate(pieces) => self.deliver(pieces),
                    };
                }
            }
        }
    }

    fn deliver(&mut self, pieces: Vec<Piece>) -> UnionAdvance {
        self.pieces.extend(pieces);
        UnionAdvance::Continue
    }
}

impl UnionChooseFrame {
    fn new(children: Vec<BodySegment>) -> Self {
        UnionChooseFrame {
            children,
            idx: 0,
            pieces: Vec::new(),
        }
    }

    fn step(&mut self, source: &str, ctx: &mut Ctx) -> UnionAdvance {
        loop {
            let Some(child) = self.children.get(self.idx) else {
                return UnionAdvance::Finished(std::mem::take(&mut self.pieces));
            };
            self.idx += 1;
            let BodySegment::DynamicTag {
                name: child_name,
                span: child_span,
            } = child
            else {
                continue;
            };
            if child_name == "when" || child_name == "otherwise" {
                let child_span = *child_span;
                let (child_segments, mut cd, _t) = capture_subtree(source, child_span);
                ctx.diagnostics.append(&mut cd);
                return match descend_union(child_segments, ctx) {
                    UnionDescend::Push(frame) => UnionAdvance::Push(frame),
                    UnionDescend::Immediate(pieces) => self.deliver(pieces),
                };
            }
        }
    }

    fn deliver(&mut self, pieces: Vec<Piece>) -> UnionAdvance {
        self.pieces.extend(pieces);
        UnionAdvance::Continue
    }
}

/// Drives `stack` to completion for the union engine -- mirrors [`run`]'s
/// own doc comment; no `Err`/short-circuit case here since this engine
/// (like `union_walk` before it) never fails.
fn run_union(mut stack: Vec<UFrame>, source: &str, ctx: &mut Ctx) -> Vec<Piece> {
    let mut incoming: Option<Vec<Piece>> = None;
    while let Some(top) = stack.last_mut() {
        let advance = match incoming.take() {
            Some(pieces) => top.deliver(pieces),
            None => top.step(source, ctx),
        };
        match advance {
            UnionAdvance::Push(frame) => stack.push(*frame),
            UnionAdvance::Continue => {}
            UnionAdvance::Finished(pieces) => {
                // See `run`'s own matching comment: `stack.pop()` is
                // always `Some` here in practice; `if let` rather than an
                // assert per CLAUDE.md rule 4.
                if let Some(frame) = stack.pop() {
                    if matches!(frame, UFrame::Union(_)) {
                        ctx.depth -= 1;
                    }
                }
                if stack.is_empty() {
                    return pieces;
                }
                incoming = Some(pieces);
            }
        }
    }
    // See `run`'s own matching fallback -- structurally unreachable in
    // practice, a safe empty result rather than a panic if it ever were.
    Vec::new()
}

/// Entry point for the union engine -- what a top-level
/// `union_walk(source, segments, ctx)` call used to be. See
/// [`run_flatten_engine`]'s own doc comment for why this still goes through
/// [`descend_union`] rather than special-casing the entry point.
fn run_union_engine(source: &str, segments: Vec<BodySegment>, ctx: &mut Ctx) -> Vec<Piece> {
    match descend_union(segments, ctx) {
        UnionDescend::Push(frame) => run_union(vec![*frame], source, ctx),
        UnionDescend::Immediate(pieces) => pieces,
    }
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
fn with_prefix(
    source: &str,
    sql: SqlString,
    wrapper_start: u32,
    strip_n: usize,
    prefix: &str,
) -> SqlString {
    // B19 (cold code review): nothing stripped and nothing prepended is a
    // genuine no-op -- return `sql` untouched. The code below used to
    // unconditionally rewrite the first span_map entry to the wrapper
    // tag's own span start regardless of `prefix`/`strip_n`, so e.g. a
    // `<foreach>` with no `open` attribute (empty `prefix`, `strip_n == 0`)
    // silently lost its inner text's own first entry -- the mapped offset
    // for the very start of the body pointed at the `<foreach>` tag
    // instead of wherever the first placeholder/text segment actually is.
    if prefix.is_empty() && strip_n == 0 {
        return sql;
    }

    let kept = &sql.text[strip_n..];
    let mut span_map = vec![(0u32, wrapper_start)];

    if !prefix.is_empty() || strip_n > 0 {
        // Find the raw offset corresponding to position `strip_n` in the
        // original text: the last span_map entry at or before `strip_n`.
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
        // B21 (cold code review): extrapolating the split point as
        // `base_raw + (strip_n - base_off)` assumes 1 decoded byte == 1 raw
        // byte across [base_off, strip_n], which is usually true (plain
        // wrapper boundary text like "AND ") but false when entity decoding
        // happened in that span (`&#x41;ND ` decodes to `AND `, 6 raw bytes
        // for 4 decoded ones). Extrapolating there would fabricate a
        // precise-looking raw offset this crate has no basis for. Verify
        // directly instead of inferring from neighboring span_map entries
        // (a following entry doesn't always exist, e.g. when the whole
        // trailing segment is coarse): the extrapolation is honest exactly
        // when the decoded slice byte-for-byte equals the corresponding
        // slice of the original source at the candidate raw offset. Byte
        // (not char) slicing throughout, so this never panics on a
        // non-char-boundary split (same rationale as A1).
        let delta = strip_n - base_off as usize;
        let candidate_raw = base_raw as usize + delta;
        let decoded_slice = sql.text.as_bytes().get(base_off as usize..strip_n);
        let source_slice = source.as_bytes().get(base_raw as usize..candidate_raw);
        let split_raw = match (decoded_slice, source_slice) {
            (Some(d), Some(s)) if d == s => candidate_raw as u32,
            _ => base_raw,
        };
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

    // B26 (cold code review): when the strip empties the body entirely
    // (e.g. `<where>AND </where>` -- the leading-AND/OR strip consumes
    // the *whole* text, so `kept` is ""), the split-point entry pushed
    // above at offset == prefix.len() sits exactly at the final text's
    // own length -- a phantom one-past-end entry describing a segment
    // that has zero surviving characters after it, same class as B9/B20.
    // `<`, not `<=`, matches with_suffix_strip's own filter.
    let text = format!("{prefix}{kept}");
    let final_len = text.len() as u32;
    span_map.retain(|(off, _)| *off < final_len);

    SqlString { text, span_map }
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
    // A9 (cold code review): defense-in-depth. Every current caller clamps
    // its own leading/trailing strip lengths so they never jointly exceed
    // the body text's length, but that clamping lives at each call site --
    // clamp here too so a future caller that forgets can't reintroduce a
    // subtract-overflow panic (debug) / out-of-bounds slice panic
    // (release) from `strip_n > sql.text.len()`.
    let strip_n = strip_n.min(sql.text.len());
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

/// Length to strip from the start for `<set>`'s leading-comma rule: a
/// comma immediately after the leading whitespace run, plus the
/// whitespace between it and the first real content -- mirrors
/// [`trailing_comma_strip_len`], reversed.
fn leading_comma_strip_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b',' {
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        j
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

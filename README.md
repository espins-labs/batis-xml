# batis-xml

**Parser and dynamic-SQL flattener for MyBatis and iBatis mapper XML.**

Legacy enterprise codebases keep years of business logic inside mapper XML —
dynamic `<if>/<choose>/<foreach>` branches, `<include>` chains, string-keyed
iBatis sqlMaps, sometimes maintained in two SQL dialects at once. Generic XML
tooling sees tags; `batis-xml` sees statements.

```text
mapper XML ──▶ batis-xml ──▶ { statements, flattened SQL variants,
                               includes, resultMaps, diagnostics }
```

## What it does

- **Parses both dialect families**: MyBatis 3 `<mapper>` and iBatis 2
  `<sqlMap>` (including namespace-less sqlMaps with `DAO.method`-style ids,
  and legacy `<dynamic>/<isNotEmpty>/<iterate>` tags).
- **Flattens dynamic SQL**: every statement yields the concrete SQL shape
  candidates with their activating conditions — so tools (and AI agents) can
  read *what actually runs* instead of mentally executing XML.
- **Never fails**: broken, truncated, half-edited XML produces partial
  results plus diagnostics — no panics, no `Err`. Built for codebases where
  "malformed" is the steady state.
- **Position-faithful**: every node carries byte spans into the decoded
  UTF-8 text (identical to original bytes for UTF-8 sources — see
  `ByteSpan`'s rustdoc for the re-encoded-document caveat); flattened SQL
  carries a span map back to the XML source.
- **Language-neutral output**: the serde JSON model is the published schema.
  Ports to other languages validate against the shipped conformance corpus
  (`fixtures/`), not against this codebase.
- **Pure Rust**: builds clean for `wasm32-unknown-unknown` — usable from
  Node/TypeScript via wasm, and from the JVM via wasm runtimes.

## Example

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let result = batis_xml::parse_bytes(&std::fs::read("OrderMapper.xml")?);
for stmt in &result.mapper.as_ref().unwrap().statements {
    println!("{:?} {:?}", stmt.kind, stmt.id);
}
# Ok(())
# }
```

## Include expansion order — read this before substituting fragments

**`<include>` markers are left in place, unexpanded** — `IncludeRef` gives
you the raw `refid` plus a best-effort `IncludeTarget`, but resolving and
substituting the referenced `<sql>` fragment's text is the consumer's job.

The marker's textual form in the flattened SQL is a **stable v1
contract**: the literal, fixed-prefix comment token
`/* batis:include(<raw>) */`, where `<raw>` is `IncludeRef.raw` verbatim
(with any literal `*/` replaced by `*_/` so it can't terminate the
comment early) — regardless of whether the target classified as `Local`,
`Qualified` (rendered with its original dot still in it, e.g.
`otherNs.frag`), or `Dynamic` (the unresolved `${...}` text rendered
as-is). A worked example:

```rust,ignore
// mapper.statements[0].sql (Variants) contains, verbatim:
//   "SELECT * FROM widget WHERE /* batis:include(widgetFilter) */"
// mapper.statements[0].includes == [Spanned { value: IncludeRef {
//   raw: "widgetFilter", target: Local("widgetFilter") }, span: ... }]
//
// To substitute: find the referenced fragment by id, flatten *it* too
// (it has its own SqlText), then string-replace the fixed-prefix token:
let token = "/* batis:include(widgetFilter) */";
let fragment_sql = "status = 'ACTIVE'"; // the matching SqlVariant's text
let substituted = sql_text.replace(token, fragment_sql);
```

Because the token's prefix (`/* batis:include(`) is fixed, a plain
substring search finds every token directly — you don't need to
reconstruct it from `raw` first, though doing so (as above) is how you
know *which* token belongs to *which* `IncludeRef` when a statement has
more than one `<include>`. Each entry in `Statement.includes`/
`SqlFragment.includes` carries a `span` — the **original XML** span of
that `<include>` element, not a position in the flattened text — which is
the same span a `DiagCode::IncludeAtWrapperBoundary` diagnostic reports
when that particular token sits at a wrapper boundary (see below).

If the fragment you're substituting is itself `SqlText::Variants` (has
its own conditions), there's no single deterministic substitution:
plug in the fragment variant whose `conditions` match the same runtime
parameter state as the *enclosing* statement's variant you're
substituting into — not an arbitrary one like `variants[0]`.

Two related contract notes, easy to miss: `Mapper::statements` (and
`fragments`/`result_maps`) preserve source document order — safe to rely
on when resolving references across statements in one file. And
diagnostic `message` strings are **not** a stable matching surface (they
may be reworded between versions without that being a breaking change) —
always match on `Diagnostic::code`, never on message text.

MyBatis (and iBatis) expand `<include>` **before** evaluating
`<where>`/`<set>`/`<trim>` dynamic semantics, so a wrapper's leading
`AND`/`OR` strip or trailing-comma strip runs against the fragment's
*actual, substituted* text. This crate flattens with the include token
still sitting where it was in the XML, so if you substitute fragment text
in **after** flattening, you must redo that part of the work yourself:

- **Re-apply the leading-AND/OR and trailing-comma cleanup** to the text
  you substitute in place of an include token that ends up first/last
  inside a `<where>`/`<set>`/`<trim>` wrapper — otherwise you can end up
  with `WHERE AND x = 1` (fragment started with `AND `) or a comma
  `<set>` was supposed to strip but never saw.
- **Treat a wrapper whose only content is an include token as
  conditional** — the fragment might expand to nothing (or to
  whitespace), in which case the whole wrapper should contribute nothing,
  exactly like an empty `<if>` branch would.

`DiagCode::IncludeAtWrapperBoundary` flags the exact spot this bites: it's
emitted whenever an `<include>` is the first or last non-whitespace direct
child of a `<where>`/`<set>`/`<trim>`, so you can find every place that
needs the extra handling above without re-deriving it from the XML by
hand. See `IncludeTarget`'s rustdoc for the same contract from the type's
point of view, and `wasm/README.md` for the npm-consumer framing.

`DiagCode::DanglingRefid` (a local `refid` with no matching `<sql>` in the
same file) is only ever emitted for **MyBatis** — it's a file-local
heuristic that has no view of any other mapper file, and iBatis fragments
are a global cross-file registry by design, so applying the same check
there would flag nearly every legitimate cross-file reference as
dangling. Even for MyBatis, a *missing* `DanglingRefid` isn't a guarantee
of resolvability: upstream MyBatis also supports cross-namespace
short-name resolution this single-file view can't see.

## Status

MM-01 through MM-14 are complete: parsing, dynamic-SQL flattening,
`<include>` resolution, `resultMap`s, placeholder normalization, encoding
detection, and hostile-input recovery are all implemented and tested.

Validated against a 195-file real-world legacy mapper corpus (MyBatis +
iBatis): 100% parse success (no panics, no `Err`), with statement/binding
accuracy of 98.9% (MyBatis) and 87.6% (iBatis) against an 85% acceptance
bar.

Pre-publish housekeeping (coverage/bench gates, schema pinning, release
automation) is tracked in [DEVELOPMENT.md](DEVELOPMENT.md).

## Use cases

- Code-intelligence indexers linking Java mapper interfaces to XML statements
- Lint/CI checks: dangling `<include refid>`, unused statements,
  **dual-dialect (oracle/mysql) drift detection**
- Migration tooling (iBatis → MyBatis → anything)
- IDE analysis panels: dynamic-SQL variant preview

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

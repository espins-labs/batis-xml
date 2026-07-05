# Development

## Design spec (single source of truth)

The internal design spec (maintained privately, not in this repo) holds the
detailed micro-feature tables, per-feature edge-case lists, invariants,
definition of done, corpus survey, and performance acceptance bar. Those
details change often; what is stable is summarized below so the id schemes
used throughout the test suite are readable without the private spec.

The **public** contract -- what ports and downstream consumers actually
conform to -- is the published JSON schema (`schema/batis-xml.v1.json`)
plus the conformance corpus in `fixtures/`.

## Id schemes used in test names

Test names carry a prefix tying them back to what they protect:

- **`mm_<nn>_*`** -- micro-feature id (the feature decomposition, table
  below). The test exercises that feature's contract.
- **`a<nn>_*` / `b<nn>_*`** -- cold-code-review finding ids. Before 0.1.0
  the crate went through seven independent adversarial review rounds;
  every finding got a sequential id (`A` = blocker/major, must fix;
  `B` = recommended fix; `C` = consciously deferred, documented in
  "Known limitations"). A test named `a18_*` is the regression guard for
  finding A18, and the commit introducing it explains the finding --
  `git log --grep 'A18'` (or the id in CHANGELOG.md) gives the story.
  The numbering is append-only across rounds, so ids are unique forever.

## Micro-features (MM-01..14)

| Id | Feature |
|---|---|
| MM-01 | Root-element identification + dialect detection (`<mapper>` = MyBatis, `<sqlMap>` = iBatis) |
| MM-02 | `namespace` attribute parsing |
| MM-03 | Statement collection (select/insert/update/delete + iBatis procedure/statement, databaseId, duplicate-id detection) |
| MM-04 | `<sql id>` fragment collection |
| MM-05 | `<include refid>` reference collection (local / qualified / dynamic) |
| MM-06 | Dynamic-tag flattening (`<if>/<choose>/<where>/<set>/<trim>/<foreach>` → per-branch SQL variants, ≤32, else union fallback) |
| MM-07 | Placeholder normalization (`#{}` → `?`, `${}` → `__BATIS_DYN__`, property-path collection) |
| MM-08 | CDATA / entity handling (verbatim SQL reconstruction) |
| MM-09 | Class-reference collection (parameterType/resultType, raw -- alias resolution is the consumer's job) |
| MM-10 | ResultMap parsing (mappings, extends, discriminator, association/collection) |
| MM-11 | iBatis dialect absorption (`<isNotEmpty>` etc., `#var#`/`$var$`, prepend) |
| MM-12 | Byte-span fidelity (every `Spanned` value slices back to its source text) |
| MM-13 | Hostile-input resilience (broken/truncated XML → partial result + diagnostics, never a panic) |
| MM-14 | Encoding detection (BOM / declared label / heuristics; reality wins over declaration) |

## Conventions

- **Test-first**, in micro-feature id order: write the failing test first.
  Test naming: `mm_<nn>_<description>` (spec↔test traceability).
- **No panics** on public paths; no `unwrap` outside tests. Every anomaly
  is a `Diagnostic`.
- **wasm-clean**: pure-Rust dependencies only; keep
  `cargo check --target wasm32-unknown-unknown` green.
- Snapshots: `insta`. Property-based: `proptest` (spec invariants 1–5).
- Coverage target 90%+ (`cargo llvm-cov`). Bench: 1 MB mapper < 50 ms
  (criterion, local Apple-silicon baseline).

## M0 implementation order

MM-01 → 02 → 03 → 08 → 04 → 05 → 07 → 06 (minimum set for accuracy
measurement) → K-1 measurement (private corpus of ~200 real mappers,
pass bar 85%).

## wasm bindings

This is a workspace: the core `batis-xml` crate (root) stays pure-Rust
(see `wasm-clean` above); `wasm/` is a separate `batis-xml-wasm` crate
(`cdylib` + `rlib`) that depends on it and adds `wasm-bindgen`. Plain
`cargo build`/`test`/`check` (no `-p`/`--workspace`) only touch the core
crate (`default-members` in the root `Cargo.toml`) -- the wasm crate is
always built explicitly.

Minimal API, JSON-string boundary (schema v1, no per-field marshalling):
`parse(bytes: &[u8]) -> String`, `detect(bytes: &[u8]) -> String` (cheap
dialect pre-check, plain string not JSON-quoted), and `version() -> String`.

Build with the one command:

```
./wasm/build.sh
node wasm/tests/smoke.js   # smoke test against the built pkg/
```

`wasm/build.sh` runs `wasm-pack build wasm --target nodejs`, then:
- copies the committed, drift-checked `wasm/schema.d.ts` (generated from
  `schema/batis-xml.v1.json` via `json-schema-to-typescript` --
  `cd wasm && npm ci && npx json2ts -i ../schema/batis-xml.v1.json -o
  schema.d.ts --unreachableDefinitions` to regenerate deliberately after a
  model change, review the diff, then commit; CI fails on any undeclared
  drift) into `pkg/schema.d.ts`
- patches `pkg/package.json`'s name from the Cargo package name
  (`batis-xml-wasm`, matching this crate's own crates.io identity -- see
  `release-plz.toml`, excluded from crates.io releases since npm is its
  channel) to the verified-available npm name (`batis-xml`), adds
  `schema.d.ts`/`LICENSE-MIT`/`LICENSE-APACHE`/`THIRD_PARTY_NOTICES`
  (the compiled binary statically links `encoding_rs`, which embeds
  WHATWG-owned encoding data under its own BSD-3-Clause license) to its
  `files` list, and writes an `exports` map (`"."` -> the JS + its
  `.d.ts`; `"./schema"` -> `schema.d.ts` only, a types-only subpath with
  no runtime file)

This produces `wasm/pkg/` (gitignored -- rebuilt on demand, not committed)
ready for `npm pack`; this repo does not automate or run `npm publish`.
`wasm/README.md` (wasm-pack copies it into `pkg/` automatically) documents
the three sharpest edges for npm consumers: feed raw bytes not
host-pre-decoded strings, spans are UTF-8 byte offsets not JS indices, and
build qualified names as `ns.id@databaseId` to avoid dual-dialect
collisions.

**npm version bump is manual** -- keep it in lockstep with the core
crate's minor releases (e.g. core `0.2.0` ships alongside npm `0.2.0`),
since the wasm crate path-depends on an exact `version = "0.1.0"`
constraint on `batis-xml` in `wasm/Cargo.toml`. `release-plz` doesn't
publish this one (see above); bumping `wasm/Cargo.toml`'s version is a
manual step when the core crate's public API changes.

## Known limitations

Deferred to 0.1.1+ deliberately, not oversights -- documented here rather
than fixed now (cold code review, section C):

- **`capture_subtree`'s O(N²) re-parse.** Flattening a dynamic tag
  re-parses its subtree from a fresh `Reader` positioned at that tag's own
  span (see its doc comment in `parse.rs`), rather than reusing segments
  already captured by an enclosing walk. For deeply/widely nested dynamic
  tags this means each nesting level re-scans everything under it, an
  O(N²) cost in the total body size. This is now **bounded** by
  `DEPTH_LIMIT` (256, see cold review B2/B3/A2) rather than unbounded, so
  it can no longer combine with pathological nesting to become
  arbitrarily slow -- it's a perf ceiling, not a correctness risk. A
  single-pass restructure (threading already-captured segments through
  instead of re-parsing) is deliberately **0.2 scope**: it rewrites the
  reviewed parsing core that every conformance fixture and proptest
  invariant is built against, and the regression risk of that rewrite
  outweighs removing a now-bounded performance ceiling that hasn't shown
  up as a real problem in the K-1 corpus or the 1MB/<50ms benchmark.

  **Quantified** (cold code review C): measured (release build, local
  Apple-silicon baseline, 3-iteration average) against a synthetic
  worst-case document -- many independent `<select>` statements, each
  wrapping its `SELECT` body in a chain of `<if>` tags nested right up to
  `DEPTH_LIMIT` (tested at both 250 and 256 deep, no meaningful
  difference), repeated to fill the target size. This parses at roughly
  **~0.32 s/MB** (vs. the ~0.05 s/MB bar for the normal-case 1 MB/<50 ms
  benchmark above -- about 6-7x slower under adversarial nesting), scaling
  **linearly with total tag count**, not with document size directly or
  with nesting depth once past the point where `DEPTH_LIMIT` caps any
  single chain -- consistent with the O(N²) cost being paid once per
  bounded-depth chain, not compounding across chains. At the 10 MiB
  `MAX_INPUT_BYTES` cap this comes to roughly **3.3 seconds**, not the
  multi-minute stalls unbounded nesting could previously produce. This
  is a real slowdown a caller might notice, but it terminates promptly
  and never approaches the 10-second range typically associated with a
  request-handling timeout.

- **`<trim>` doesn't replicate MyBatis's own whole-body `.trim()` step**
  (cold code review B41, minor, whitespace-only, span-preserving).
  Upstream MyBatis's `TrimSqlNode`/`FilteredDynamicContext.applyAll()`
  always calls Java's `String.trim()` on the *entire* accumulated body
  text first -- unconditionally, whether or not `prefixOverrides`/
  `suffixOverrides` end up matching anything -- before checking length
  and applying `prefix`/`suffix`. This crate's `expand_trim` (flatten.rs)
  only strips leading/trailing whitespace as *part of* an override match
  (`leading_override_strip_len`/`trailing_override_strip_len` skip
  whitespace before checking the candidate token, so a matched override
  already consumes any whitespace ahead of it correctly) -- but when
  *no* override matches, or none is configured at all, surrounding
  whitespace inside the `<trim>` body is left exactly as authored. For
  example, `<trim prefix="(" suffix=")">\n  widget_name\n</trim>`
  becomes `"( \n  widget_name\n )"` here, not MyBatis's tightly-wrapped
  `"(widget_name)"`. Never changes SQL keyword/identifier content and
  never corrupts a span (every offset still points at real source
  bytes; nothing here fabricates or coarsens one) -- purely extra
  whitespace bytes surviving where upstream would have dropped them.
  Accepted as a known 0.1 gap rather than a blocking fix; deferred to
  0.1.1+ (a real fix means threading a whole-body trim through
  `with_prefix`/`with_suffix_strip`'s span-preserving strip machinery,
  which is more surgery than this divergence's practical impact -- most
  real mapper SQL doesn't rely on exact inter-token whitespace --
  currently justifies).

## Pre-publish checklist

- [x] Generate `schema/batis-xml.v1.json` and pin it with a snapshot test
- [x] Conformance corpus: 15+ MyBatis / 8+ iBatis (3+ dual-dialect pairs) / hostile set
- [x] License review (MIT alone vs MIT OR Apache-2.0 dual) — dual-licensed
- [x] Wire up cargo-semver-checks and release-plz
- [x] Decide whether wasm bindings ship as a separate `batis-xml-wasm` crate — yes, `wasm/` in this workspace

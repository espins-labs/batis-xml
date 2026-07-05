# Development

## Design spec (single source of truth)

The internal design spec (maintained privately, not in this repo) holds the
micro-feature tables (MM-01..14), invariants, definition of done, corpus
survey, and performance acceptance bar (round-3 target: answer the chain
question in 1 call, ≤ 6,315 bytes of context, 4/4 recall).

The **public** contract -- what ports and downstream consumers actually
conform to -- is the published JSON schema (`schema/batis-xml.v1.json`)
plus the conformance corpus in `fixtures/`.

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
  `schema.d.ts`/`LICENSE-MIT`/`LICENSE-APACHE` to its `files` list, and
  writes an `exports` map (`"."` -> the JS + its `.d.ts`; `"./schema"` ->
  `schema.d.ts` only, a types-only subpath with no runtime file)

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

## Pre-publish checklist

- [x] Generate `schema/batis-xml.v1.json` and pin it with a snapshot test
- [x] Conformance corpus: 15+ MyBatis / 8+ iBatis (3+ dual-dialect pairs) / hostile set
- [x] License review (MIT alone vs MIT OR Apache-2.0 dual) — dual-licensed
- [x] Wire up cargo-semver-checks and release-plz
- [x] Decide whether wasm bindings ship as a separate `batis-xml-wasm` crate — yes, `wasm/` in this workspace

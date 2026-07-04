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
`parse(bytes: &[u8]) -> String` and `version() -> String`.

Build with:

```
wasm-pack build wasm --target nodejs
node wasm/tests/smoke.js   # smoke test against the built pkg/
```

This produces `wasm/pkg/` (gitignored -- rebuilt on demand, not committed).
The npm package name (`batis-xml`, availability verified) differs from the
Cargo package name (`batis-xml-wasm`, matching this crate's own crates.io
identity); `wasm-pack` writes the Cargo name into `pkg/package.json`, so
renaming it is a manual step before `npm pack` -- this repo does not
automate or run `npm publish`.

## Pre-publish checklist

- [x] Generate `schema/batis-xml.v1.json` and pin it with a snapshot test
- [x] Conformance corpus: 15+ MyBatis / 8+ iBatis (3+ dual-dialect pairs) / hostile set
- [x] License review (MIT alone vs MIT OR Apache-2.0 dual) — dual-licensed
- [x] Wire up cargo-semver-checks and release-plz
- [x] Decide whether wasm bindings ship as a separate `batis-xml-wasm` crate — yes, `wasm/` in this workspace

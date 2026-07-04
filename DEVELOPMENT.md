# Development

## Design spec (single source of truth)

the internal design spec (maintained privately) — micro-feature tables
MM-01..14, invariants, definition of done. (Working name `mybatis-mapper`
in those documents = this crate, published as `batis-xml`.)

Related: corpus survey the internal corpus survey; performance
acceptance bar the internal benchmark plan (round-3 target:
answer the chain question in 1 call, ≤ 6,315 bytes of context, 4/4 recall).

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

## Pre-publish checklist

- [x] Generate `schema/batis-xml.v1.json` and pin it with a snapshot test
- [ ] Conformance corpus: 15+ MyBatis / 8+ iBatis (3+ dual-dialect pairs) / hostile set
- [x] License review (MIT alone vs MIT OR Apache-2.0 dual) — dual-licensed
- [ ] Wire up cargo-semver-checks and release-plz
- [ ] Decide whether wasm bindings ship as a separate `batis-xml-wasm` crate

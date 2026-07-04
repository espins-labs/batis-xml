# batis-xml — agent working agreement

Parser + dynamic-SQL flattener for MyBatis/iBatis mapper XML. Public API and
output model in `src/model.rs` are **final** — implement the parser behind
them; do not change model fields without explicit approval. One exception:
**adding `DiagCode` variants is allowed** (the schema is additive there),
but every addition must be called out explicitly in your status report so
the spec can be kept in sync.

## Method (non-negotiable)

1. **Test-first, micro-feature order**: MM-01 → 02 → 03 → 08 → 04 → 05 →
   07 → 06 (then 09–14). One micro-feature at a time: write the failing
   test, implement minimally, refactor. Never implement ahead of tests.
2. **Test naming**: `mm_<nn>_<description>` — the spec id prefix is
   mandatory (traceability).
3. **Spec is the single source of truth**:
   the internal design spec (maintained privately) (micro-feature tables,
   edge cases, invariants). Read the relevant MM row *before* writing each
   test. Working name `mybatis-mapper` there = this crate.
4. **No panics** on public paths, no `unwrap`/`expect` outside tests. Every
   anomaly becomes a `Diagnostic`. `parse`/`parse_bytes` never return `Err`.
5. **Spans are original-byte offsets**, never decoded-string offsets.
6. **English only** in code comments, docs, and commit messages.

## Git safety

Never run destructive git commands (`checkout --`, `restore`,
`reset --hard`) while uncommitted unique work exists — make a WIP commit
first (amend/squash later).

## Gates (run before claiming done)

```
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test
cargo check --target wasm32-unknown-unknown   # pure-Rust deps only
```

## Fixtures

- `fixtures/` pairs are the portable spec. **Synthetic only** — never
  commit content derived from proprietary code (imitate patterns, invent
  identifiers/statements).
- `expected.json` is generated via review-and-approve (insta), never
  hand-written. `fixtures/hostile/` has no expected output — only the
  never-panics invariant.
- proptest-regressions/ must be committed if it appears.

## Acceptance context

- K-1 gate: ≥85% statement/binding accuracy on a private real-world corpus
  (measured outside this repo).
- Performance bar: parse a 1 MB mapper in <50 ms (criterion, local).
- Downstream consumers read `SqlText` — flattening quality (MM-06) and
  placeholder normalization (MM-07) are the product, not extras.

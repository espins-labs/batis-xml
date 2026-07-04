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
- **Position-faithful**: every node carries original byte spans; flattened
  SQL carries a span map back to the XML source.
- **Language-neutral output**: the serde JSON model is the published schema.
  Ports to other languages validate against the shipped conformance corpus
  (`fixtures/`), not against this codebase.
- **Pure Rust**: builds clean for `wasm32-unknown-unknown` — usable from
  Node/TypeScript via wasm, and from the JVM via wasm runtimes.

## Example (target API)

```rust
let result = batis_xml::parse_bytes(&std::fs::read("OrderMapper.xml")?);
for stmt in &result.mapper.as_ref().unwrap().statements {
    println!("{:?} {:?}", stmt.kind, stmt.id);
}
```

## Status

Scaffold — public API and output model are final; the parser is being built
micro-feature-first with test-first development. Do not depend on this yet.

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

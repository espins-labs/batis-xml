# Conformance Corpus

Pairs of `{name}.xml` (input) and `{name}.expected.json` (expected output).
**This corpus IS the portable spec** — implementations in other languages
conform to these pairs, not to the Rust code.

## Layout

- `mybatis/` — MyBatis 3 mapper DTD. Coverage targets: dynamic tags
  (`<if>/<choose>/<foreach>`), comparison operators inside CDATA,
  `<include>` chains, resultMaps.
- `ibatis/` — iBatis 2 sqlMap DTD. Namespace-less sqlMaps with
  `DAO.method`-style embedded id prefixes, `<dynamic>/<isNotEmpty>/<iterate>`,
  and **at least 3 dual-dialect (oracle/mysql) statement pairs**.
- `hostile/` — corrupted input (unclosed tags, truncation, non-XML). No
  expected output — only the "never panics" invariant is checked.

## Authoring rules

1. **Synthetic only**: fixtures are written by structural imitation
   (reproduce the *patterns*; invent every statement, schema, and
   identifier). Never commit content derived from real proprietary code.
2. `expected.json` is never written by hand — it is generated through a
   review-and-approve flow once the parser lands.
3. One fixture represents one pattern (the file name describes the pattern).

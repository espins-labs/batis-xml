# Conformance Corpus

Pairs of `{name}.xml` (input) and `{name}.expected.json` (expected output).
**This corpus IS the portable spec** — implementations in other languages
conform to these pairs, not to the Rust code.

## Layout

- `mybatis/` — MyBatis 3 mapper DTD. Coverage targets: dynamic tags
  (`<if>/<choose>/<foreach>`), comparison operators inside CDATA (see
  `comparison_operators_in_cdata.xml`), `<include>` chains, resultMaps.
- `ibatis/` — iBatis 2 sqlMap DTD. Namespace-less sqlMaps with
  `DAO.method`-style embedded id prefixes, `<dynamic>/<isNotEmpty>/<iterate>`,
  and **at least 3 dual-dialect (oracle/mysql) statement pairs** (currently:
  `minimal_dual_dialect_*`, `dual_dialect_pagination_*`,
  `dual_dialect_upsert_*` — each pair is two separate physical files with
  the same statement id, matching iBatis 2's per-database sqlMap-file
  convention, not MyBatis 3's `databaseId` attribute mechanism).
- `hostile/` — corrupted input (unclosed tags, truncation, non-XML). No
  expected output — only the "never panics" invariant is checked.

Diagnostic `message` strings and `Union.branch_count` values in
`expected.json` are **normative, byte-for-byte, for ports** — a port that
resolves an anomaly differently but produces a different message text
still fails conformance. (`message` text is *not*, however, a stable
matching surface for a *consumer* of this crate's own output -- match on
`code` there; normativity here is about what a from-scratch port's output
must equal, a different question.)

**Exception (B40, cold code review):** a `message` that embeds a
recovery/dependency-derived error's own `Display` text verbatim is
*not* part of that normative surface — only the `code` and `span` are,
for these specific diagnostics. Two fixtures currently do this:
`orphan_closing_tag_recovery.expected.json` (`UnclosedTag`, embedding
`quick_xml::Error`'s own message for the orphan/mismatched-tag
recovery path) and `entity_heavy_text.expected.json` (`InvalidEntity`,
embedding `quick_xml::escape::EscapeError`'s message for the
unresolvable-entity case). A from-scratch port necessarily uses a
*different* XML tokenizer with its own error prose — requiring it to
reproduce this crate's specific dependency's wording byte-for-byte
would make the "spec" churn on every quick-xml version bump (see the
A10/A18 CHANGELOG entries) for no benefit to a port, which has no such
dependency to match in the first place.

## Authoring rules

1. **Synthetic only**: fixtures are written by structural imitation
   (reproduce the *patterns*; invent every statement, schema, and
   identifier). Never commit content derived from real proprietary code.
2. `expected.json` is never written by hand — it is generated through a
   review-and-approve flow once the parser lands.
3. One fixture represents one pattern (the file name describes the pattern).

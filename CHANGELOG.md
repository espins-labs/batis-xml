# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: minor version bumps may include additive breaking changes to the
Rust API surface, per `src/model.rs`'s header doc comment; the JSON schema
itself stays additive within v1 — see `schema/README.md`).

From the first crates.io release onward, entries below this stub are
maintained by `release-plz` (see `release-plz.toml`) from conventional
commit messages, not hand-written.

## [0.1.1](https://github.com/espins-labs/batis-xml/compare/v0.1.0...v0.1.1) - 2026-07-11

### Fixed

- replace depth-guarded recursion with heap worklists (small-stack overflow) ([#2](https://github.com/espins-labs/batis-xml/pull/2))

### Other

- *(package)* exclude private-corpus scratch harnesses from the published crate
- *(examples)* take the sqlMap subpath from IBATIS_SQLMAP_SUBPATH
- *(readme)* add crates.io/docs.rs/npm/CI/license badges + docs.rs link; drop internal MM ids from Status

## [0.1.0] - Unreleased

Initial release: MyBatis 3 and iBatis 2 mapper XML parsing, dynamic-SQL
flattening (`<if>`/`<choose>`/`<where>`/`<set>`/`<trim>`/`<foreach>` and
their iBatis equivalents), `<include>` resolution, `resultMap` mapping
collection, placeholder normalization, encoding detection, and
hostile-input recovery (no panics, no `Err`). See `README.md` for the
full feature list and `DEVELOPMENT.md` for the pre-publish checklist.

Hardened through three internal pre-publish reviews before the first
tag: fixed several panics on multibyte/pathological input (char-boundary
slicing, unbounded recursion, trim-strip overlap), corrected dynamic-SQL
flattening gaps (`<trim>` prefix/suffix spacing, `<set>` leading-comma
strip, `<selectKey>` statement splitting, iBatis `##`/`$$` escapes),
added forward-compat enum/schema openness guarantees, added the
`ParseResult.encoding` field, and expanded diagnostics coverage
(`IncludeAtWrapperBoundary`, `NestingLimitExceeded`, unrecognized-element
detection). None of this changed the public model's field/variant names
except the additive `DiagCode`/model-field growth already covered by
`schema/README.md`'s additive-within-v1 policy.

Correction to the quick-xml 0.37 → 0.41 bump's original changelog claim
("degrading gracefully ... exactly as before"): a fourth review found
that claim was wrong for one input shape. quick-xml 0.41's
`allow_dangling_amp` defaults to `false`, so a bare `&` (or an
unterminated reference like `&amp` without a `;`) made the reader error
out and drop everything up to the next `<` -- silently losing SQL text
and any placeholder inside it, and reporting `UnclosedTag` instead of
`InvalidEntity`. Fixed by enabling `allow_dangling_amp` on every
`Reader` this crate constructs and diagnosing the dangling case
explicitly (raw text is kept verbatim, per the same MM-08 rule already
applied to unresolvable named references).

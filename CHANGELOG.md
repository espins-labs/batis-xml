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

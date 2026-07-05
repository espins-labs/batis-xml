# Output Schema

`batis-xml.v1.json` — the JSON Schema for the output model
(`src/model.rs`), generated via `schemars` behind the off-by-default
`schema` cargo feature and pinned by `tests/schema_v1.rs`. Regenerate
deliberately on a model change, review the diff, then commit the updated
file — the test fails on any undeclared drift.

Ports (implementations in other languages) conform to this schema plus the
conformance corpus in `fixtures/`. Field additions are non-breaking;
removals/renames require a v2.

**Enum openness (2026-07-05, cold code-review contract audit):**
- `DiagCode` values may be added within v1 itself — **validators must pass
  unknown codes through** rather than rejecting them; this schema file is
  updated with each addition. On the Rust side this is enforced by
  `#[non_exhaustive]` plus a `#[serde(other)]` fallback variant.
- `SqlText` may also gain a new representation within v1 (`#[non_exhaustive]`
  on the Rust side) — treat an unrecognized shape as "can't render this
  variant" rather than a hard error.
- `Dialect`, `StatementKind`, and `IncludeTarget` are **closed sets** —
  their existing "other/generic/dynamic" member already covers the
  escape-hatch case, so an exhaustive match/switch on these three is safe
  to rely on. Adding a member to any of them would be a v2.

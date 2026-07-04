# Output Schema

`batis-xml.v1.json` — the JSON Schema for the output model
(`src/model.rs`), generated via `schemars` behind the off-by-default
`schema` cargo feature and pinned by `tests/schema_v1.rs`. Regenerate
deliberately on a model change, review the diff, then commit the updated
file — the test fails on any undeclared drift.

Ports (implementations in other languages) conform to this schema plus the
conformance corpus in `fixtures/`. Field additions are non-breaking;
removals/renames require a v2.

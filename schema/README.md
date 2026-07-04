# Output Schema

`batis-xml.v1.json` — the JSON Schema for the output model
(`src/model.rs`) as serialized by serde. **Generated and pinned by snapshot
tests once the parser lands** — not yet generated.

Ports (implementations in other languages) conform to this schema plus the
conformance corpus in `fixtures/`. Field additions are non-breaking;
removals/renames require a v2.

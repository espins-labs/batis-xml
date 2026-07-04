# Contributing to batis-xml

Thanks for your interest! This document is short — please read all of it,
especially the fixture policy, which is a hard rule.

## ⚠️ Fixture and sample policy (hard rule)

**Never submit XML derived from proprietary code** — not in fixtures, not
in issue reports, not in test snippets. Many users of this library work on
closed enterprise codebases; pasting a real mapper (even "just one
statement") into a public issue can leak schema names, business logic, and
SQL that isn't yours to publish.

Instead, **write a synthetic reproduction**: keep the *structure* that
triggers the behavior (tags, nesting, placeholders), invent every
identifier, table, and column (`demo_widget`, `grp_cd`, …). Maintainers
will decline contributions that look like real-world dumps, even good ones.

## Workflow

- Trunk-based: `main` is the only long-lived branch. Fork the repo, create
  a topic branch (`fix/…`, `feat/…`), open a PR against `main`.
- **PRs are squash-merged**, and the PR title becomes the commit message.
  Title must follow [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat:`, `fix:`, `docs:`, `test:`, `chore:`) — release automation
  derives versions and the changelog from it.
- Keep PRs small and single-purpose. Test-first is how this codebase is
  built; a PR that adds behavior without a test that fails beforehand will
  be asked to add one.

## Before opening a PR

Run the full gate locally:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                       # also runs conformance + hostile suites
cargo check --target wasm32-unknown-unknown
```

## Project conventions

- **No panics on public paths** — no `unwrap`/`expect` outside tests.
  Every anomaly is reported as a `Diagnostic`; `parse`/`parse_bytes` never
  return `Err`.
- **Test naming**: unit tests are prefixed with the micro-feature id they
  cover (`mm_07_…`) for traceability. New behavior extends the existing
  numbering scheme.
- **English only** in code comments, docs, and commit messages.
- **Spans are original-byte offsets.** If your change touches text
  handling, preserve span integrity (the property tests will catch you if
  not — run them).

## Model and schema changes

The output model (`src/model.rs`) is a published contract:

- Serialization is pinned as `schema/batis-xml.v1.json`; the conformance
  corpus in `fixtures/` is the portable spec that ports validate against.
- **Additions only**: new fields need `#[serde(default)]`; removing or
  renaming anything is a breaking change and needs a maintainer discussion
  first. Adding `DiagCode` variants is fine — call it out in the PR
  description.
- After any model change: regenerate the schema (feature-gated test) and
  every affected `expected.json`/snapshot, review the diffs, and commit
  them with the change.

## Licensing

This project is dual-licensed under MIT OR Apache-2.0. Unless you
explicitly state otherwise, any contribution intentionally submitted for
inclusion shall be dual-licensed as above, without any additional terms or
conditions (see README). No CLA.

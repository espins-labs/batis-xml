<!-- PR title must be a Conventional Commit (feat:/fix:/docs:/test:/chore:)
     — it becomes the squash-merge commit message and drives release notes. -->

## What & why

<!-- One or two sentences. Link the issue if there is one. -->

## Checklist

- [ ] Local gates pass: `cargo fmt --check` / `clippy -D warnings` / `cargo test` / `cargo check --target wasm32-unknown-unknown`
- [ ] New behavior has a test that failed before the change (test name uses the `mm_XX_` prefix scheme)
- [ ] **All XML in this PR is synthetic** — no proprietary-derived content (see CONTRIBUTING)
- [ ] If the model changed: schema + affected `expected.json`/snapshots regenerated and reviewed; `DiagCode` additions called out below

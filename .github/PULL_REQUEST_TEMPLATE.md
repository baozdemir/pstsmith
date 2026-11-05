## What and why

<!-- What this changes, and the reason. Link the issue if there is one. -->

## Checks

- [ ] A test that fails before this change and passes after it (for a fix).
- [ ] `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`.
- [ ] If outputs change: `tests/fixtures/expected.sha256` rewritten and the change explained here.
- [ ] A CHANGELOG entry under `[Unreleased]`.
- [ ] No real mail anywhere in this pull request.

---
name: release
description: Release a new outlook-pst-rw version to crates.io — version bump, CHANGELOG, verification, PR, publish, tag. Use when asked to release, publish, or bump outlook-pst-rw.
---

# Release outlook-pst-rw

1. **Pick the version** (SemVer): fix → patch, new API → minor.
2. On the feature branch, bump `version` in `crates/pst/Cargo.toml` and add to
   `crates/pst/CHANGELOG.md` a `## [X.Y.Z] - YYYY-MM-DD` section under `## [Unreleased]`
   (`### Added` / `### Changed` / `### Fixed`; say the symptom and numbers, not just the change).
3. **Verify:**
   - `cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings`
   - `cargo publish -p outlook-pst-rw --dry-run`
   - For any change to the on-disk format or the commit path: SCANPST clean and Outlook opens the
     `gen-fixture-pst` output, its `--stress` output and a large batched append (Windows).
4. Commit `chore: release outlook-pst-rw X.Y.Z`, push, open a PR into `main`. The PR body follows
   Summary / Numbers (if perf) / Test plan checkboxes.
5. After merge, publish **from the merged, clean `main`**:
   ```sh
   git switch main && git pull && git status --short   # must be empty
   cargo publish -p outlook-pst-rw
   git tag outlook-pst-rw-vX.Y.Z && git push origin outlook-pst-rw-vX.Y.Z
   ```
   Never publish from a dirty tree: 1.2.9 was published with uncommitted changes and needed a
   later "sync source with published 1.2.9" commit.
6. Delete the merged branch (local and remote).

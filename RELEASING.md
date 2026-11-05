# Releasing pstsmith

For maintainers. A release publishes the crates `pstsmith` and `pstsmith-cli` to crates.io, a GitHub release `vX.Y.Z`
with attested binaries, the npm packages `pstsmith` and `@pstsmith/<platform>`, and the Homebrew formula.

## One-time setup

1. crates.io: publish 0.1.0 by hand from a clean checkout (`cargo publish -p pstsmith`, then `cargo publish -p
   pstsmith-cli`), then add this repository as a trusted publisher of both crates (crates.io > crate > Settings >
   Trusted Publishing: workflow `release-plz.yml`, environment `release`).
2. npm: create the organisation `pstsmith`; publish the first versions by hand or add the repository as a trusted
   publisher of `pstsmith` and of each `@pstsmith/<platform>` package (workflow `release.yml`, environment `release`).
3. GitHub: create the environment `release` (Settings > Environments), optionally with required reviewers; set the
   repository variable `RELEASE_ENABLED` to `true`. Immutable releases can be on: every release is a draft until its
   archives are attached.
4. Homebrew: create the repository `baozdemir/homebrew-tap` and a fine-grained token with contents write on it only;
   store it as the secret `HOMEBREW_TAP_TOKEN` of the `release` environment.
5. The first GitHub release, after the crates are on crates.io (release-plz makes the later ones):

   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   gh release create v0.1.0 --draft --verify-tag --title v0.1.0 --notes-file <notes>
   gh workflow run release.yml -f tag=v0.1.0
   ```

   `release.yml` builds the tag for seven targets, attaches the archives, their attestations and `SHA256SUMS` to the
   draft, publishes it, then publishes the npm packages and the Homebrew formula.

## Every later release

release-plz opens a pull request with the version bump and the changelog. It opens it with the workflow's own token, and
GitHub starts no workflow for such a pull request, so CI does not run on it by itself: close and reopen it (or push
to its branch) to run CI, or give release-plz a GitHub App token as its documentation describes
(release-plz.ieni.dev/docs/github/token). Merging it publishes the crates (trusted publishing), tags `vX.Y.Z` and
creates the GitHub release as a draft; `release-plz.yml` then calls `release.yml`, which attaches the binaries for seven
targets, their attestations and `SHA256SUMS`, publishes the release, and publishes the npm packages and the Homebrew
formula.

## Before tagging

- CI is green on `main`, including the fixture byte identity on Linux, macOS and Windows and the libpff oracle.
- A change that changes output bytes says so in the changelog, with how its output was checked
  ([docs/VERIFICATION.md](docs/VERIFICATION.md)).

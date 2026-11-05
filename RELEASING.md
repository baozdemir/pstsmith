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
   repository variable `RELEASE_ENABLED` to `true`.
4. Homebrew: create the repository `baozdemir/homebrew-tap` and a fine-grained token with contents write on it only;
   store it as the secret `HOMEBREW_TAP_TOKEN`.
5. The first GitHub release: `gh release create v0.1.0 --title v0.1.0 --notes-file <notes>` (release-plz creates the
   later ones). It starts `release.yml`.

## Every later release

Merge the pull request release-plz opens: it holds the version bump and the changelog. The merge publishes the crates
(trusted publishing), tags `vX.Y.Z` and creates the GitHub release, which starts `release.yml`: binaries for seven
targets, their attestations and `SHA256SUMS`, the npm packages, and the Homebrew formula.

## Before tagging

- CI is green on `main`, including the fixture byte identity on Linux, macOS and Windows and the libpff oracle.
- A change that changes output bytes says so in the changelog, with how its output was checked
  ([docs/VERIFICATION.md](docs/VERIFICATION.md)).

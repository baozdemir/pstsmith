# Releasing pstsmith

For maintainers. A release publishes the crates `pstsmith` and `pstsmith-cli` to crates.io, a GitHub release `vX.Y.Z`
with attested binaries, the npm packages `pstsmith` and `@pstsmith/<platform>`, and the Homebrew formula.

## One-time setup

1. crates.io: publish 0.1.0 by hand from a clean checkout (`cargo publish -p pstsmith`, then `cargo publish -p
   pstsmith-cli`), then add this repository as a trusted publisher of both crates (crates.io > crate > Settings >
   Trusted Publishing: workflow `release-plz.yml`, environment `release`).
2. npm: create the organisation `pstsmith`. A package's trusted publisher can be set only once the package exists, so
   the first release publishes with a token: create a granular access token that can publish `pstsmith` and
   `@pstsmith/*`, and store it as the secret `NPM_TOKEN` of the `release` environment. Just before the second
   release (npm expires a trusted publisher that has not published within 2 days of its creation), add this
   repository as the trusted publisher of `pstsmith` and of each `@pstsmith/<platform>` package (workflow
   `release-plz.yml`: npm checks the calling workflow, and `release.yml` is always called from it; environment
   `release`). Keep the token until one release has published through them, then delete it and the secret. An
   expired trusted publisher is deleted and created again.
3. GitHub: create the environment `release` (Settings > Environments); set the repository variable `RELEASE_ENABLED`
   to `true`; allow GitHub Actions to create pull requests (Settings > Actions > General), which release-plz needs
   for its release pull request. Required reviewers on `release` would hold every push to `main`, since release-plz's publish job runs
   on each push (it publishes only when a version is new). Immutable releases can be on: every release is a draft until its
   archives are attached.
4. Homebrew: create the repository `baozdemir/homebrew-tap` and a fine-grained token with contents write on it only;
   store it as the secret `HOMEBREW_TAP_TOKEN` of the `release` environment.
5. The first GitHub release, after the crates are on crates.io (release-plz makes the later ones):

   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   gh release create v0.1.0 --draft --verify-tag --title v0.1.0 --notes-file <notes>
   gh workflow run release-plz.yml -f tag=v0.1.0
   ```

   `release.yml` builds the tag for seven targets, attests the archives and `SHA256SUMS` (`gh attestation verify`),
   attaches them to the draft, publishes it, then publishes the npm packages and the Homebrew formula. The same
   command re-runs the binaries of any release whose draft is still unpublished.

## Every later release

release-plz opens a pull request with the version bump and the changelog. It opens it with the workflow's own token, and
GitHub starts no workflow for such a pull request, so CI does not run on it by itself: close and reopen it (or push
to its branch) to run CI, or give release-plz a GitHub App token as its documentation describes
(release-plz.ieni.dev/docs/github/token). Merging it publishes the crates (trusted publishing), tags `vX.Y.Z` and
creates the GitHub release as a draft; `release-plz.yml` then calls `release.yml`, which attests and attaches the
binaries for seven targets and `SHA256SUMS`, publishes the release, and publishes the npm packages and the Homebrew
formula.

## Before tagging

- CI is green on `main`, including the fixture byte identity on Linux, macOS and Windows and the libpff oracle.
- A change that changes output bytes says so in the changelog, with how its output was checked
  ([docs/VERIFICATION.md](docs/VERIFICATION.md)).

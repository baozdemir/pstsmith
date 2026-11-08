# The same bytes on every platform

The same input gives the same PST file, byte for byte, whatever system pstsmith runs on.

| File | What | Build | Date | System |
| --- | --- | --- | --- | --- |
| `corpus-macos-arm64.sha256` | SHA-256 of the 36 outputs of 21 test manifests: 21 PST files and the 15 exclusion records of the v2 manifests | pstsmith 0.1.0 | 2026-11-08 | macOS 15.6.1, Apple M4 Pro |
| `corpus-linux-arm64.sha256` | the same 36 outputs | a build of the research workspace pstsmith came from, Rust 1.99.1 | 2026-11-02 | Linux arm64, in Docker on the same Mac (kernel 6.10.14-linuxkit) |
| `corpus-linux-x86-64.sha256` | the same 36 outputs | the same research build | 2026-11-03 | Linux x86-64, a Google Cloud VM (kernel 7.0.0-1014-gcp) |
| `fixtures-v0.1.0.log` | `cargo test -p pstsmith --test fixtures`: every fixture gives the bytes in `tests/fixtures/expected.sha256` | pstsmith 0.1.0 | 2026-11-08 | macOS 15.6.1, Apple M4 Pro |

The warning at the top of `fixtures-v0.1.0.log` is about that machine's Rust toolchain (its `rust-objcopy` could not
load a library, so debug information was not stripped), not about pstsmith.

The three hash lists are identical. 31 of the 36 outputs come from the manifests in `tests/fixtures/manifest` and
equal their entries in `tests/fixtures/expected.sha256`; the other five (`v1-interleaved`, `v1-scale`,
`v1-throughput`, and `v2-interleaved-as-v2` with its exclusion record) come from test manifests of the research
workspace that are not published.

Two checks in this repository keep it so:

- CI runs the fixture test (`crates/pstsmith/tests/fixtures.rs`) on Linux, macOS and Windows on every change.
- The release workflow builds `pstsmith` for seven targets (Linux x86-64 and arm64 with glibc and with musl, macOS
  arm64 and x86-64, Windows x86-64) and converts the 8 mail fixtures with each binary before anything is published;
  every output must equal its entry in `expected.sha256` (`.github/workflows/release.yml`, the step "The binary gives
  the fixtures' bytes"). For v0.1.0 the step passed on all seven
  ([the release run](https://github.com/baozdemir/pstsmith/actions/runs/42230966509), 2026-11-06; GitHub keeps run logs
  for a limited time).

## Repeat it

From the repository root, on any system:

    cargo test -p pstsmith --test fixtures --release --locked

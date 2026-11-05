# Contributing to pstsmith

Thank you for helping. Bug reports, fixes, documentation and new test cases are all welcome.

## Never attach real mail

PST, EML and MBOX files hold people's private mail. Never attach a real mailbox to an issue or a pull request, not even
a "small" one. Describe the problem, or make a synthetic file that shows it (addresses in `example.invalid`), as the
fixtures under `tests/fixtures` do.

## Building and testing

```sh
python3 tests/fixtures/generate.py                     # once: writes the 64 MiB attachment some fixtures name
cargo test --workspace                                 # unit, integration and fixture tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace
```

The independent checks need libpff's Python binding (`pip install libpff-python`):

```sh
cargo build --release --workspace
python3 tests/oracle/compare_mail.py target/release/pstsmith
sh tests/oracle/structure.sh path/to/output.pst
```

## Changes that change output bytes

`crates/pstsmith/tests/fixtures.rs` compares the SHA-256 of every fixture output with `tests/fixtures/expected.sha256`,
on Linux, macOS and Windows. A change that changes what pstsmith writes must say so: rewrite the list with
`PSTSMITH_BLESS=1 cargo test -p pstsmith --test fixtures`, and explain in the pull request which outputs change and
why. A change to the writer itself (the `ndb`, `ltp` and `messaging` modules, the session) also needs the output read
back by libpff and, where it touches what Outlook reads, a note of how it was checked; [docs/VERIFICATION.md](docs/VERIFICATION.md)
says what the existing checks cover.

## Fixing a bug

Add a test that fails before the fix and passes after it, in the same pull request. Keep a fix to the smallest change
that addresses the cause.

## Pull requests

- One topic per pull request; describe what changes and why.
- Run the commands above; CI runs them too, on all three platforms.
- Add an entry under `[Unreleased]` in [CHANGELOG.md](CHANGELOG.md) (or `crates/pstsmith-cli/CHANGELOG.md`).
- Contributions are accepted under the project's [MIT license](LICENSE). No contributor license agreement is needed.

## Code of conduct

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

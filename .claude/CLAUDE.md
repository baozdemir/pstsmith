# outlook-pst-rs (fork)

Fork of [microsoft/outlook-pst-rs](https://github.com/microsoft/outlook-pst-rs) that adds Unicode PST
creation and incremental append. Published to crates.io as **`outlook-pst-rw`** (lib name `outlook_pst`).
Main consumer is Memex's mail → PST export, so performance is measured with memex-like exports.

## Layout

- `crates/pst/src/lib.rs` — the writer (create / append / commit, ~7k lines). Almost all fork work lives here.
- `crates/pst/src/{ndb,ltp,messaging}` — upstream reader. Change only when the writer needs it.
- `crates/compressed-rtf` — upstream, untouched.
- `crates/pst/examples/gen-fixture-pst.rs` — fixture generator (`-- out.pst`, `-- out.pst --stress`).

## Checks (same as CI on Linux/macOS/Windows)

```sh
cargo fmt --check && cargo build && cargo test && cargo clippy -- -D warnings
```

Passing tests is not enough for format changes: a PST must also be **SCANPST clean** (no `!!`) and
**open in Outlook** (COM) on Windows. Run both on the fixture, the `--stress` fixture and a large
batched append before releasing.

## MS-PST rules we learned the hard way

- Every HN allocation, row index and property-tree node must be ≤ **3580 bytes**; larger values go to
  subnodes. Violations show up as SCANPST *Fatal Error 80040813*, not as test failures.
- Large contents-table cells go to **`NID_TYPE_LTP`** subnodes; other subnode types make Outlook show
  an empty folder.
- Header `rgbFM` / `rgbFP` are filled with `0xFF`.
- Commits are copy-on-write: new data only goes to space the committed AMap marks free, and the
  header switches roots last. Keep that ordering; it is what makes a crashed append safe.
- Not supported (by design): ANSI PST append, encoded/encrypted PSTs, reclaiming freed holes.

## Conventions

- Commit messages: `feat:` / `fix:` / `perf:` / `chore:`, describing the user-visible effect.
- `crates/pst/CHANGELOG.md` follows Keep a Changelog; each entry says *why* (symptom, numbers).
- Commit as the personal identity (`Dong SHIN <dong_s@me.com>`), not a company account.
- Branch → PR into `main` → delete the branch after merge.
- Releases: use the `release` skill.

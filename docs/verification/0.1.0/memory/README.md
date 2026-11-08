# Peak memory

How much memory pstsmith needs as its input grows. The figure is the process's maximum resident set size
(`ru_maxrss`, what `/usr/bin/time -l` prints as "maximum resident set size"); MB here is 1,000,000 bytes.

- **Where and when:** pstsmith 0.1.0, release build, 2026-11-08, macOS 15.6.1 on an Apple M4 Pro.

## Many items: `items-run-1.jsonl`, `items-run-2.jsonl`

`pstsmith --manifest ITEMS.jsonl --output FILE.pst --batch 128` on two generated manifests (`pst-export-manifest/1`),
twice each. The items have no attachments and a body of 200 bytes.

| Case | Items | Run 1 | Run 2 |
| --- | --- | --- | --- |
| `one-128000` | 128,000 in one folder | 15.84 MB | 15.07 MB |
| `many-1024` | 32,000 in 1,024 folders (each folder's items one after another) | 14.52 MB | 15.27 MB |

`max_rss` is the figure above. The fields about commits and the heap are 0 or null: they come from a build with
measurement features, which was not used here.

## Large attachments: `attachments.jsonl`

`pstsmith --manifest ... --output ...` with `/usr/bin/time -l`, three times each. The attachments are a 1.5 GiB file
(1,610,612,736 bytes of a fixed pattern); a message in a PST cannot hold an attachment of 2 GiB or more.

| Series | Input | Peak (3 runs) |
| --- | --- | --- |
| `metadata-only` | one message without an attachment | 3.92 to 4.10 MB |
| `sat-1x1536m` | one message with a 1.5 GiB attachment, `--batch 1` | 5.91 to 6.41 MB |
| `sat-4x1536m` | four messages with a 1.5 GiB attachment each (6 GiB in all; a 6.7 GB file), `--batch 1` | 7.24 to 7.55 MB |

## A 2 GiB MBOX: `mbox-2gib.test.log`, `mbox-2gib.time.txt`

The ignored test `mbox_streams_without_reading_whole_file` (`crates/pstsmith/tests/convert_mail.rs`) writes a 2 GiB
MBOX whose first message is 300 MiB and the rest 4 MiB each, converts it, and checks the result. Its test binary ran
under `/usr/bin/time -l`: the test passed with a peak of 792 MB (792,068,096 bytes). The converter holds one message
at a time, so the peak follows the largest message.

## Repeat it

From the repository root (the code of tag v0.1.0 for the same figures):

    cargo build --release --locked -p pstsmith-cli
    /usr/bin/time -l target/release/pstsmith --manifest ITEMS.jsonl --output out.pst --batch 128

with a manifest as described above ([docs/MANIFEST.md](../../../MANIFEST.md) has the format). For the MBOX:

    cargo test -p pstsmith --release --locked --test convert_mail --no-run
    /usr/bin/time -l target/release/deps/convert_mail-<hash> --ignored --exact mbox_streams_without_reading_whole_file

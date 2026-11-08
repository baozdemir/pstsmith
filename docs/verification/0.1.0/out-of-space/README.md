# Running out of disk space

The question: when the disk fills up during an append or a conversion, does pstsmith stop with an error, leave a file
it was appending to at its last commit, and leave nothing behind of a conversion?

- **Where and when:** pstsmith 0.1.0, 2026-11-08, macOS 15.6.1 on an Apple M4 Pro.
- **The disk:** [run_full_disk.py](run_full_disk.py) gives every run a fresh copy of an empty 16 MB HFS+ disk image,
  attached with `hdiutil` (no administrator rights needed) and removed after the run. A filler file fills the volume
  and is then cut short, which leaves exactly the bytes it was cut by free. HFS+ and not APFS: a 16 MB APFS volume
  keeps about 1 MB to itself and does not give space back at once, so its free space cannot be set.

## Appends: 100 runs

A file of 30 messages, written by [../crash-safety/probe](../crash-safety/probe/), is copied onto the volume, the
filler leaves 0 to 1,216,512 bytes free (in steps of 12 KiB), and the probe appends batches of 10 messages, one commit
each, until the disk is full.

As the file is left, the probe must have stopped with `No space left on device (os error 28)`, `pst-table-check`
must pass, the file must hold the messages of the batches the probe acknowledged or those and the failed batch, and
`pst-ref-check` must pass if the header marks the allocation map valid (see [../crash-safety](../crash-safety/) for
why). Then the filler is removed, the writer opens the file again and appends one more batch, and both checks must
pass with 10 more messages.

**100 of 100 passed** (`summary.txt`; `results.jsonl` has one line per run):

- Every run stopped with `No space left on device`: 66 while writing a message's blocks (the error names the message
  and the file size), 34 in the commit after them. Before that, 0 batches were acknowledged in 21 runs, 1 in 21, 2 in
  41 and 3 in 17.
- In all 100 the file held the state before the failed batch, its header marked the map valid, and both checks passed
  as it was left. The writer sets the file's new length before it writes the header that commits (`commit` in
  `crates/pstsmith/src/session.rs`), and HFS+ allocates that length when it is set, so the disk ran out before the
  commit point every time.
- The file was left longer than its header says, by 3,072 to 429,056 bytes: what the failed commit had written past
  the committed end. Nothing in the file refers to it. After the next commit, the reopen's, the file's length equals
  the end its header gives in all 100.
- After the filler was removed, all 100 opened in the writer, took one more batch and passed both checks.

## Conversions: 6 runs

`pstsmith convert` of a 19.5 MB MBOX (40 messages, each with a 360 KB attachment of random bytes; the script writes it)
onto the volume with 1, 4 and 12 MB free, each with its staging directory (`--temp-dir`) once on another disk and once
on the full volume. Each must exit with status 5 and leave no output file on the volume and nothing in the staging
directory.

**6 of 6 passed.** With the staging directory on another disk, the writer ran out of space (`"error":"writer"`, at
messages 2, 11 and 34); with it on the full volume, staging the decoded messages did (`"error":"io"`, `no storage
space`).

## What this does not show

- HFS+ allocates a file's new length when it is set. On a file system that allocates when the bytes are written, such
  as APFS or ext4, the disk may also run out after the commit point, while the writer writes the allocation map. The
  file then holds the new commit with its map marked invalid, the state the kills in
  [../crash-safety](../crash-safety/) left 288 times and the writer repaired each time; running out of space at that
  point was not tested.
- One machine.

## Repeat it

On macOS, from the repository root. The run above used the code of tag v0.1.0; this folder was added after the tag
and changes no code. The work directory needs about 70 MB.

    cargo build --workspace --release --locked
    cargo build --release --manifest-path docs/verification/0.1.0/crash-safety/probe/Cargo.toml \
        --target-dir target/probe
    python3 -I docs/verification/0.1.0/out-of-space/run_full_disk.py target/release/pstsmith \
        target/probe/release/append-probe target/release /tmp/full-disk > results.jsonl 2> summary.txt
